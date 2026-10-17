//! Building an image: options → layers → size → blocks on a device.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::device::{Device, FileDevice, MemDevice};
use crate::error::{Error, IoContext, Result};
use crate::fs::{BytesSource, ChunkSource, FileSource, Fs, NewImage, show};
use crate::hash::FxHashMap;
use crate::layers::{Log, Policy, Scanner, TarData, open_tar};
use crate::ondisk::*;
use crate::options::{ImageInfo, LayerKind, Options, Report, Source, Timestamps};
use crate::prefetch::{PrefetchedSource, Prefetcher};
use crate::size::{self, Content, Params, lost_found_blocks};
use crate::source::tar::TarReader;
use crate::tree::{Body, DataSrc, Meta, NodeId, ROOT, Tree, dir_content};

/// Where the image goes.
pub enum Output<'a> {
    /// A file (created, replaced, or updated in place when it is the
    /// starting image).
    Path(&'a Path),
    /// A stream, such as a pipe.
    Writer(&'a mut dyn Write),
}

/// Build an image into a file.
pub fn build_file(options: &Options, path: impl AsRef<Path>) -> Result<Report> {
    build(options, Output::Path(path.as_ref()))
}

/// Build an image in memory.
pub fn build_vec(options: &Options) -> Result<(Vec<u8>, Report)> {
    let (v, r) = run(options, Target::Memory)?;
    Ok((v.unwrap_or_default(), r))
}

/// Build an image to a file or stream.
pub fn build(options: &Options, output: Output<'_>) -> Result<Report> {
    match output {
        Output::Path(p) => Ok(run(options, Target::File(p))?.1),
        Output::Writer(w) => {
            let (v, r) = build_vec(options)?;
            w.write_all(&v)
                .and_then(|_| w.flush())
                .ctx(|| "writing the image".to_string())?;
            Ok(r)
        }
    }
}

/// Build an image on a device (its previous content is replaced, or
/// replaced by the starting image).
pub fn build_on_device(options: &Options, device: &mut dyn Device) -> Result<Report> {
    Ok(run(options, Target::Device(device))?.1)
}

enum Target<'a> {
    Memory,
    File(&'a Path),
    Device(&'a mut dyn Device),
}

/// Checked options.
struct Settings {
    bs: u32,
    reserve_pct: f64,
    bytes_per_inode: f64,
    fs_time: u32,
    fill: u8,
    creator_os: u32,
}

fn validate(o: &Options) -> Result<Settings> {
    if ![1024, 2048, 4096].contains(&o.block_size) {
        return Err(Error::usage(format!(
            "invalid block size {}: it must be 1024, 2048 or 4096",
            o.block_size
        )));
    }
    let fraction = f64::from(o.reserved_fraction.unwrap_or(0.05));
    let reserve_pct = (fraction * 100.0 * 1e6).round() / 1e6;
    if !reserve_pct.is_finite() || reserve_pct < 0.0 {
        return Err(Error::usage(format!(
            "invalid reserved percentage {reserve_pct}"
        )));
    }
    if reserve_pct >= 100.0 {
        return Err(Error::usage(format!(
            "invalid reserved percentage {reserve_pct}: it must be below 100"
        )));
    }
    let bytes_per_inode = f64::from(o.bytes_per_inode.unwrap_or(16384.0));
    if !bytes_per_inode.is_finite() || bytes_per_inode <= 0.0 {
        return Err(Error::usage(format!(
            "invalid bytes per inode {bytes_per_inode}"
        )));
    }
    if o.creator_os < 0 {
        return Err(Error::usage(format!("invalid creator OS {}", o.creator_os)));
    }
    if let Some(n) = o.size_in_blocks {
        if n < 0 {
            return Err(Error::usage(format!("invalid size in blocks {n}")));
        }
    }
    if let Some(n) = o.number_of_inodes {
        if n < 0 {
            return Err(Error::usage(format!("invalid number of inodes {n}")));
        }
    }
    let stdin = o
        .layers
        .iter()
        .map(|l| &l.source)
        .chain(o.starting_image.as_ref())
        .filter(|s| matches!(s, Source::Stdin))
        .count();
    if stdin > 1 {
        return Err(Error::usage("only one input can come from standard input"));
    }
    for l in &o.layers {
        if l.kind == LayerKind::Dir && !matches!(l.source, Source::Path(_)) {
            return Err(Error::usage("directory layers need a path"));
        }
    }
    let fs_time = match o.timestamps {
        Timestamps::Zero => 0,
        Timestamps::Fixed(t) | Timestamps::Clamp(t) => t,
        Timestamps::Source => source_time(o.source_date_epoch)?,
    };
    Ok(Settings {
        bs: o.block_size,
        reserve_pct,
        bytes_per_inode,
        fs_time,
        fill: o.fill_value as u8,
        creator_os: o.creator_os as u32,
    })
}

fn source_time(sde: Option<i64>) -> Result<u32> {
    let t = match sde {
        Some(t) => t,
        None => match std::env::var("SOURCE_DATE_EPOCH") {
            Ok(v) => v
                .trim()
                .parse::<i64>()
                .map_err(|_| Error::usage(format!("invalid SOURCE_DATE_EPOCH '{v}'")))?,
            Err(_) => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        },
    };
    u32::try_from(t).map_err(|_| Error::usage(format!("timestamp {t} out of range")))
}

/// Phase timing, with `MKEXT2_TIMING` set.
struct Timer {
    on: bool,
    last: Instant,
}

impl Timer {
    fn new() -> Self {
        Timer {
            on: std::env::var_os("MKEXT2_TIMING").is_some(),
            last: Instant::now(),
        }
    }

    fn lap(&mut self, what: &str) {
        if self.on {
            let now = Instant::now();
            eprintln!("mkext2: {what}: {:.3} s", (now - self.last).as_secs_f64());
            self.last = now;
        }
    }
}

fn read_stdin() -> Result<Arc<[u8]>> {
    let mut v = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut v)
        .ctx(|| "reading standard input".to_string())?;
    Ok(v.into())
}

fn read_source(s: &Source) -> Result<Arc<[u8]>> {
    match s {
        Source::Path(p) => Ok(std::fs::read(p)
            .map_err(|e| Error::io(p.display().to_string(), e))?
            .into()),
        Source::Bytes(b) => Ok(b.clone()),
        Source::Stdin => read_stdin(),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn open_output(p: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
        .map_err(|e| Error::io(p.display().to_string(), e))
}

fn run(options: &Options, target: Target<'_>) -> Result<(Option<Vec<u8>>, Report)> {
    let mut timer = Timer::new();
    let st = validate(options)?;
    let mut log = Log {
        sink: options.message_sink.clone(),
        messages: Vec::new(),
    };
    // standard input is read into memory: it cannot be read twice
    let mut layers = options.layers.clone();
    for l in &mut layers {
        if matches!(l.source, Source::Stdin) {
            if options.size_in_blocks.is_none() && options.starting_image.is_none() {
                log.info("reading standard input into memory to size the image");
            }
            l.source = Source::Bytes(read_stdin()?);
        }
    }
    let policy = Policy {
        timestamps: options.timestamps,
        squash_uid: options.squash_uids,
        squash_perms: options.squash_perms,
    };

    let mut mem: Option<MemDevice> = None;
    let mut file: Option<FileDevice> = None;

    if let Some(start) = &options.starting_image {
        log.info(format!("starting from image {}", start.describe()));
        let dev: &mut dyn Device = match target {
            Target::Memory => {
                mem = Some(MemDevice::from_vec(read_source(start)?.to_vec()));
                mem.as_mut().unwrap()
            }
            Target::File(p) => {
                let in_place = matches!(start, Source::Path(sp) if same_file(sp, p));
                if !in_place {
                    match start {
                        Source::Path(sp) => {
                            std::fs::copy(sp, p).map_err(|e| {
                                Error::io(format!("copying {} to {}", sp.display(), p.display()), e)
                            })?;
                        }
                        other => {
                            let data = read_source(other)?;
                            std::fs::write(p, &data[..])
                                .map_err(|e| Error::io(p.display().to_string(), e))?;
                        }
                    }
                }
                file = Some(FileDevice::new(open_output(p)?));
                file.as_mut().unwrap()
            }
            Target::Device(d) => {
                let data = read_source(start)?;
                d.set_len(0)?;
                d.set_len(data.len() as u64)?;
                d.write_at(0, &data)?;
                d
            }
        };
        let mut fs = Fs::open(dev)?;
        if options.block_size_set && options.block_size != fs.bs() {
            return Err(Error::usage(format!(
                "the block size {} differs from the starting image's ({})",
                options.block_size,
                fs.bs()
            )));
        }
        let mut tree = Tree::open(&mut fs)?;
        let bs = fs.bs();
        let prefetch = scan(
            options,
            &layers,
            &mut tree,
            &mut log,
            Some(&mut fs),
            policy,
            bs,
        )?;
        timer.lap("reading the inputs");
        let report = write(
            options, &st, &layers, tree, &mut log, &mut fs, prefetch, false, &mut timer,
        )?;
        drop(fs);
        if let Some(f) = file {
            f.into_inner().ctx(|| "writing the image".to_string())?;
        }
        return Ok((mem.map(MemDevice::into_vec), report));
    }

    // a new image
    let mut tree = Tree::new(Meta {
        perm: 0o755,
        uid: 0,
        gid: 0,
        atime: st.fs_time,
        ctime: st.fs_time,
        mtime: st.fs_time,
    });
    let prefetch = scan(options, &layers, &mut tree, &mut log, None, policy, st.bs)?;
    timer.lap("reading the inputs");

    let content = Content::of(&tree, st.bs);
    let params = Params {
        bs: st.bs,
        reserve_pct: st.reserve_pct,
        bytes_per_inode: st.bytes_per_inode,
        min_inodes: options.number_of_inodes.unwrap_or(0).max(0) as u64,
    };
    let layout = match options.size_in_blocks {
        Some(n) => {
            let (l, raised) = size::fixed(n as u64, &content, &params)?;
            if raised {
                log.info(format!(
                    "size raised from {n} to {} blocks to fit the content",
                    l.geo.blocks
                ));
            }
            l
        }
        None => size::compute(&content, &params, |needed| {
            options.readjustment.apply(needed)
        })?,
    };
    if layout.lost_found {
        tree.create_first(
            ROOT,
            b"lost+found",
            Meta {
                perm: 0o700,
                uid: 0,
                gid: 0,
                atime: st.fs_time,
                ctime: st.fs_time,
                mtime: st.fs_time,
            },
        );
    }
    timer.lap("sizing");

    let dev: &mut dyn Device = match target {
        Target::Memory => {
            mem = Some(MemDevice::new());
            mem.as_mut().unwrap()
        }
        Target::File(p) => {
            file = Some(FileDevice::new(open_output(p)?));
            file.as_mut().unwrap()
        }
        Target::Device(d) => d,
    };
    let mut fs = Fs::create(
        dev,
        NewImage {
            geo: layout.geo.clone(),
            reserved: layout.reserved,
            time: st.fs_time,
            creator_os: st.creator_os,
            label: options.volume_label.as_deref(),
        },
    )?;
    let report = write(
        options,
        &st,
        &layers,
        tree,
        &mut log,
        &mut fs,
        prefetch,
        layout.lost_found,
        &mut timer,
    )?;
    drop(fs);
    if let Some(f) = file {
        f.into_inner().ctx(|| "writing the image".to_string())?;
        timer.lap("flushing");
    }
    Ok((mem.map(MemDevice::into_vec), report))
}

/// Read the layers; returns the files to read ahead.
fn scan(
    options: &Options,
    layers: &[crate::options::Layer],
    tree: &mut Tree,
    log: &mut Log,
    fs: Option<&mut Fs<'_>>,
    policy: Policy,
    bs: u32,
) -> Result<Vec<(Arc<Path>, u64)>> {
    let mut sc = Scanner::new(tree, log, fs, policy, bs, options.allow_holes);
    for (i, layer) in layers.iter().enumerate() {
        let target = sc.target(layer.target.as_deref())?;
        let tpath = layer.target.clone().unwrap_or_else(|| b"/".to_vec());
        let what = layer.source.describe();
        match layer.kind {
            LayerKind::Dir => {
                let Source::Path(p) = &layer.source else {
                    unreachable!("checked")
                };
                sc.log.info(format!("adding directory {what}"));
                sc.add_dir(p, target, &tpath)?;
            }
            LayerKind::Tar => {
                sc.log.info(format!("adding archive {what}"));
                sc.add_tar(i, &layer.source, target)?;
            }
            LayerKind::DevTable => {
                sc.log.info(format!("adding device table {what}"));
                let (text, times) = match &layer.source {
                    Source::Path(p) => {
                        let md = std::fs::metadata(p)
                            .map_err(|e| Error::io(format!("{}: not found", p.display()), e))?;
                        if !md.is_file() {
                            return Err(Error::fatal(format!(
                                "{}: not a regular file",
                                p.display()
                            )));
                        }
                        let text =
                            std::fs::read(p).map_err(|e| Error::io(p.display().to_string(), e))?;
                        let times = file_times(&md);
                        (Arc::<[u8]>::from(text), times)
                    }
                    Source::Bytes(b) => (b.clone(), (0, 0)),
                    Source::Stdin => unreachable!("read into memory"),
                };
                sc.add_devtable(&text, times, target)?;
            }
        }
    }
    Ok(std::mem::take(&mut sc.prefetch))
}

fn file_times(md: &std::fs::Metadata) -> (i64, i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (md.ctime(), md.mtime())
    }
    #[cfg(not(unix))]
    {
        let m = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (m, m)
    }
}

/// The archive being read while writing.
struct TarState {
    layer: usize,
    reader: TarReader,
    next: u64,
}

#[allow(clippy::too_many_arguments)]
fn write(
    options: &Options,
    st: &Settings,
    layers: &[crate::options::Layer],
    mut tree: Tree,
    log: &mut Log,
    fs: &mut Fs<'_>,
    prefetch: Vec<(Arc<Path>, u64)>,
    lost_found: bool,
    timer: &mut Timer,
) -> Result<Report> {
    let bs = fs.bs();
    // block maps: resolve the paths first
    let mut maps: Vec<(Vec<u8>, NodeId)> = Vec::new();
    for p in &options.block_maps {
        match tree.resolve_path(ROOT, p, Some(&mut *fs))? {
            Some(id) => maps.push((p.clone(), id)),
            None => {
                return Err(Error::fatal(format!("block map: {} not found", show(p))));
            }
        }
    }
    let wanted: FxHashMap<NodeId, ()> = maps.iter().map(|&(_, id)| (id, ())).collect();

    // with a starting image, check the space first
    if fs.existing {
        let mut blocks = 0u64;
        let mut inodes = 0u64;
        for id in 0..tree.nodes.len() {
            let n = &tree.nodes[id];
            if !n.existing {
                inodes += 1;
                blocks += size::node_blocks(&tree, id, bs);
            } else if n.kind == S_IFDIR && tree.dir(id).changed {
                blocks += size::node_blocks(&tree, id, bs);
            }
        }
        if blocks > fs.free_blocks() || inodes > fs.free_inodes() {
            return Err(Error::fatal(format!(
                "the starting image is too small: the content needs {blocks} blocks and {inodes} inodes, {} and {} are free",
                fs.free_blocks(),
                fs.free_inodes()
            )));
        }
    }

    // inode numbers, in creation order (lost+found first)
    let lf_id = lost_found.then(|| tree.dir(ROOT).entries[0].1);
    let order: Vec<NodeId> = lf_id
        .into_iter()
        .chain((0..tree.nodes.len()).filter(|&i| Some(i) != lf_id))
        .collect();
    for &id in &order {
        let n = &tree.nodes[id];
        if n.existing || n.ino != 0 {
            continue;
        }
        let dir = n.kind == S_IFDIR;
        tree.nodes[id].ino = fs.alloc_inode(dir)?;
    }

    // more readers than this mostly wait on each other opening files
    let threads = options.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(4)
    });
    let prefetcher = if threads > 1 {
        Prefetcher::start(prefetch, threads)
    } else {
        None
    };
    let mut tar: Option<TarState> = None;
    let mut block_lists: FxHashMap<NodeId, (u64, Vec<u32>)> = FxHashMap::default();
    let mut large_file = false;
    // one inode each: sized once instead of growing (and copying) by halves
    let new_nodes = order.iter().filter(|&&id| !tree.nodes[id].existing).count();
    fs.table.reserve(new_nodes);

    for &id in &order {
        if tree.nodes[id].existing {
            continue;
        }
        let want_map = wanted.contains_key(&id);
        let mut inode = inode_meta(&tree, id)?;
        let n = &tree.nodes[id];
        let lay = match &n.body {
            Body::Dir(d) => {
                let extra = if Some(id) == lf_id {
                    lost_found_blocks(bs) as usize - 1
                } else {
                    0
                };
                let content = dir_bytes(&tree, id, d, bs, fs.filetype, extra);
                inode.set_size64(content.len() as u64);
                Some(fs.write_bytes(&content, want_map)?)
            }
            Body::File(f) => {
                let size = f.size;
                inode.set_size64(size);
                if size >= 1 << 31 {
                    large_file = true;
                }
                let lay = fs.plan(size, f.present.as_deref(), want_map)?;
                match &f.src {
                    DataSrc::Empty => {
                        fs.write_data(&lay, size, &mut BytesSource { data: &[], pos: 0 })?;
                    }
                    DataSrc::Path { path: p, prefetch } => {
                        let name = p.as_os_str().as_encoded_bytes();
                        let head = prefetcher
                            .as_ref()
                            .and_then(|pf| Some((pf, pf.take(*prefetch)?)));
                        match head {
                            Some((prefetcher, Ok(head))) => {
                                let mut src = PrefetchedSource {
                                    prefetcher,
                                    head: Some(head),
                                    pos: 0,
                                    done: 0,
                                    path: p,
                                    limit: size,
                                    rest: None,
                                    name,
                                };
                                fs.write_data(&lay, size, &mut src)?;
                            }
                            Some((_, Err(e))) => {
                                return Err(Error::io(format!("reading {}", p.display()), e));
                            }
                            None => {
                                let file = std::fs::File::open(p).map_err(|e| {
                                    Error::io(format!("reading {}", p.display()), e)
                                })?;
                                let mut src = FileSource {
                                    file,
                                    limit: size,
                                    done: 0,
                                    name,
                                };
                                fs.write_data(&lay, size, &mut src)?;
                            }
                        }
                    }
                    DataSrc::Tar { layer, member } => {
                        let (layer, member) = (*layer, *member);
                        if tar.as_ref().is_none_or(|t| t.layer != layer) {
                            tar = Some(TarState {
                                layer,
                                reader: open_tar(&layers[layer].source)?,
                                next: 0,
                            });
                        }
                        let t = tar.as_mut().unwrap();
                        while t.next <= member {
                            if t.reader.next_entry()?.is_none() {
                                return Err(Error::fatal(format!(
                                    "{}: the archive changed while it was read",
                                    layers[layer].source.describe()
                                )));
                            }
                            t.next += 1;
                        }
                        let mut src = TarData {
                            reader: &mut t.reader,
                            left: size,
                        };
                        fs.write_data(&lay, size, &mut src as &mut dyn ChunkSource)?;
                    }
                }
                Some(lay)
            }
            Body::Symlink(t) => {
                inode.set_size64(t.len() as u64);
                if t.len() < FAST_SYMLINK_MAX {
                    inode.block_bytes_mut()[..t.len()].copy_from_slice(t);
                    None
                } else {
                    Some(fs.write_bytes(t, want_map)?)
                }
            }
            Body::Device(major, minor) => {
                let (a, b) = encode_device(*major, *minor);
                inode.set_block(0, a);
                inode.set_block(1, b);
                None
            }
            Body::Other => None,
        };
        if let Some(lay) = lay {
            for (i, &b) in lay.i_block.iter().enumerate() {
                inode.set_block(i, b);
            }
            inode.set_blocks((lay.nblocks * u64::from(bs / 512)) as u32);
            if let Some(m) = lay.map {
                block_lists.insert(id, (inode.size64(), m));
            }
        }
        fs.table.insert(tree.nodes[id].ino, inode);
    }
    drop(prefetcher);
    drop(tar);
    timer.lap("writing the content");

    // existing nodes that changed
    if fs.existing {
        for id in 0..tree.nodes.len() {
            let n = &tree.nodes[id];
            if !n.existing {
                continue;
            }
            let changed_dir = n.kind == S_IFDIR && tree.dir(id).changed;
            if !n.dirty && !changed_dir {
                continue;
            }
            let ino = n.ino;
            let mut inode = fs.inode(ino)?;
            let mode = (u32::from(inode.mode()) & S_IFMT) | n.perm;
            inode.set_mode(mode as u16);
            inode.set_uid(n.uid);
            inode.set_gid(n.gid);
            inode.set_atime(n.atime);
            inode.set_ctime(n.ctime);
            inode.set_mtime(n.mtime);
            inode.set_links_count(links(n.links, n.kind)?);
            if changed_dir {
                let (data, meta) = fs.inode_blocks(&inode)?;
                let old: Vec<u32> = data.into_iter().filter(|&b| b != 0).chain(meta).collect();
                fs.free_later(&old);
                let content = dir_bytes(&tree, id, tree.dir(id), bs, fs.filetype, 0);
                let lay = fs.write_bytes(&content, wanted.contains_key(&id))?;
                inode.set_size64(content.len() as u64);
                for (i, &b) in lay.i_block.iter().enumerate() {
                    inode.set_block(i, b);
                }
                inode.set_blocks((lay.nblocks * u64::from(bs / 512)) as u32);
                if let Some(m) = lay.map {
                    block_lists.insert(id, (inode.size64(), m));
                }
            }
            fs.table.insert(ino, inode);
        }
        if let Some(l) = &options.volume_label {
            fs.sb.set_volume_name(l);
        }
        if options.creator_os != 0 {
            fs.sb.set_creator_os(st.creator_os);
        }
    }
    if large_file {
        if fs.sb.rev_level() < DYNAMIC_REV {
            return Err(Error::fatal(
                "files of 2 GiB or more need a revision 1 starting image",
            ));
        }
        let ro = fs.sb.feature_ro_compat() | FEATURE_RO_COMPAT_LARGE_FILE;
        fs.sb.set_feature_ro_compat(ro);
    }

    // block maps
    for (path, id) in &maps {
        let (size, blocks) = match block_lists.remove(id) {
            Some(v) => v,
            None => {
                let ino = fs.inode(tree.nodes[*id].ino)?;
                let (data, _) = fs.inode_blocks(&ino)?;
                (ino.size64(), data)
            }
        };
        let mut text = format!("{size}:");
        for b in &blocks {
            text.push(' ');
            text.push_str(&b.to_string());
        }
        text.push('\n');
        // keep the list for a path given twice
        block_lists.insert(*id, (size, blocks));
        let name: Vec<u8> = path
            .iter()
            .map(|&c| if c == b'/' { b'_' } else { c })
            .chain(b".blk".iter().copied())
            .collect();
        let dir = options
            .block_map_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let file = dir.join(path_from_bytes(&name));
        std::fs::write(&file, text).map_err(|e| Error::io(file.display().to_string(), e))?;
    }

    fs.finish(st.fill)?;
    timer.lap("writing the metadata");

    let sb = &fs.sb;
    let info = ImageInfo {
        block_size: bs,
        blocks: sb.blocks_count(),
        free_blocks: sb.free_blocks_count(),
        reserved_blocks: sb.r_blocks_count(),
        inodes: sb.inodes_count(),
        free_inodes: sb.free_inodes_count(),
        groups: fs.geo.groups,
    };
    let verbose_output = if options.verbose {
        tree.load_all(Some(&mut *fs))?;
        Some(describe(&tree, fs, &info)?)
    } else {
        None
    };
    Ok(Report {
        info,
        messages: std::mem::take(&mut log.messages),
        verbose_output,
    })
}

fn links(n: u32, kind: u32) -> Result<u16> {
    let max = if kind == S_IFDIR { 32000 } else { 65000 };
    if n > max {
        return Err(Error::fatal(format!("too many links ({n}) to one inode")));
    }
    Ok(n as u16)
}

/// A new node's inode, without its blocks.
fn inode_meta(tree: &Tree, id: NodeId) -> Result<Inode> {
    let n = &tree.nodes[id];
    let mut i = Inode::default();
    i.set_mode((n.kind | n.perm) as u16);
    i.set_uid(n.uid);
    i.set_gid(n.gid);
    i.set_atime(n.atime);
    i.set_ctime(n.ctime);
    i.set_mtime(n.mtime);
    i.set_links_count(links(n.links, n.kind)?);
    Ok(i)
}

/// The blocks of a directory.
fn dir_bytes(
    tree: &Tree,
    id: NodeId,
    d: &crate::tree::Dir,
    bs: u32,
    filetype: bool,
    extra: usize,
) -> Vec<u8> {
    let me = tree.nodes[id].ino;
    let parent = tree.nodes[d.parent].ino;
    let entries = [
        (b".".to_vec(), me, FT_DIR),
        (b"..".to_vec(), parent, FT_DIR),
    ]
    .into_iter()
    .chain(d.entries.iter().map(|(name, c)| {
        let n = &tree.nodes[*c];
        (name.to_vec(), n.ino, dirent_file_type(n.kind))
    }));
    dir_content(entries, bs as usize, filetype, extra)
}

#[cfg(unix)]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).into_owned())
}

fn mode_string(mode: u32) -> String {
    let t = match mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        S_IFCHR => 'c',
        S_IFBLK => 'b',
        S_IFIFO => 'p',
        S_IFSOCK => 's',
        _ => '-',
    };
    let mut s = String::from(t);
    for (shift, special, lower, upper) in [
        (6, 0o4000, 's', 'S'),
        (3, 0o2000, 's', 'S'),
        (0, 0o1000, 't', 'T'),
    ] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        let x = bits & 1 != 0;
        s.push(match (mode & special != 0, x) {
            (true, true) => lower,
            (true, false) => upper,
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// The `-v` description: geometry, groups and every entry.
fn describe(tree: &Tree, fs: &mut Fs<'_>, info: &ImageInfo) -> Result<String> {
    use std::fmt::Write as _;
    let mut s = String::new();
    let geo = fs.geo.clone();
    let _ = writeln!(
        s,
        "{} blocks of {} bytes ({} free, {} reserved), {} inodes ({} free), {} groups of {} blocks and {} inodes",
        info.blocks,
        info.block_size,
        info.free_blocks,
        info.reserved_blocks,
        info.inodes,
        info.free_inodes,
        info.groups,
        geo.bpg,
        geo.ipg
    );
    let _ = writeln!(
        s,
        "label {:?}",
        show(fs.sb.volume_name().split(|&c| c == 0).next().unwrap_or(b""))
    );
    for g in 0..geo.groups {
        let start = geo.group_start(g);
        let end = start + geo.group_len(g) - 1;
        let sb = if geo.has_super(g) {
            format!(
                ", superblock {start}, descriptors {}-{}",
                start + 1,
                start + geo.gdt_blocks
            )
        } else {
            String::new()
        };
        let it = geo.inode_table(g);
        let _ = writeln!(
            s,
            "group {g}: blocks {start}-{end}{sb}, block bitmap {}, inode bitmap {}, inode table {}-{}",
            geo.block_bitmap(g),
            geo.inode_bitmap(g),
            it,
            it + geo.itable_blocks - 1
        );
    }
    let _ = writeln!(
        s,
        "{:>8} {:<10} {:>5} {:>6} {:>6} {:>12} path",
        "inode", "mode", "links", "uid", "gid", "size"
    );
    for (path, id) in tree.paths() {
        let n = &tree.nodes[id];
        let ino = fs.inode(n.ino)?;
        let mut line = format!(
            "{:>8} {:<10} {:>5} {:>6} {:>6} {:>12} {}",
            n.ino,
            mode_string(u32::from(ino.mode())),
            ino.links_count(),
            ino.uid32(),
            ino.gid32(),
            ino.size64(),
            show(&path)
        );
        match &n.body {
            Body::Symlink(t) => {
                let _ = write!(line, " -> {}", show(t));
            }
            Body::Device(ma, mi) => {
                let _ = write!(line, " ({ma}, {mi})");
            }
            _ => {}
        }
        s.push_str(&line);
        s.push('\n');
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::mode_string;

    #[test]
    fn modes() {
        assert_eq!(mode_string(0o40755), "drwxr-xr-x");
        assert_eq!(mode_string(0o104755), "-rwsr-xr-x");
        assert_eq!(mode_string(0o41777), "drwxrwxrwt");
        assert_eq!(mode_string(0o120777), "lrwxrwxrwx");
    }
}
