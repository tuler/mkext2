//! Reading the layers into the [`Tree`]: directories, tar archives and
//! device tables, with the ownership, permission and time policies.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::fs::{ChunkSource, FileSource, Fs, scan_holes, show};
use crate::hash::FxHashMap;
use crate::ondisk::*;
use crate::options::{Level, Message, MessageSink, Source, Timestamps};
use crate::source::dir::{self as host, lstat, may_read, read_sorted};
use crate::source::tar::{TarEntry, TarReader};
use crate::tree::{Body, DataSrc, FileData, Meta, NodeId, ROOT, Tree, components, join};

/// Collected messages.
pub(crate) struct Log {
    pub sink: Option<MessageSink>,
    pub messages: Vec<Message>,
}

impl Log {
    pub fn push(&mut self, level: Level, text: String) {
        let m = Message { level, text };
        if let Some(s) = &self.sink {
            s(&m);
        }
        self.messages.push(m);
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.push(Level::Info, text.into());
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.push(Level::Warning, text.into());
    }
}

/// How input metadata is turned into inode metadata.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    /// The filesystem time.
    pub fs_time: u32,
    pub timestamps: Timestamps,
    pub squash_uid: Option<u32>,
    pub squash_perms: bool,
}

impl Policy {
    pub fn time(&self, t: i64) -> u32 {
        if self.fs_time == 0 {
            return 0;
        }
        match self.timestamps {
            Timestamps::Source => t as u32,
            Timestamps::Zero => 0,
            Timestamps::Fixed(f) => f,
            Timestamps::Clamp(c) => t.clamp(0, i64::from(c)) as u32,
        }
    }

    /// Times (atime, ctime, mtime) from an input's ctime and mtime.
    pub fn times(&self, ctime: i64, mtime: i64) -> (u32, u32, u32) {
        let m = self.time(mtime);
        (m, self.time(ctime), m)
    }

    /// Metadata for an entry of a directory or archive layer.
    pub fn meta(&self, kind: u32, perm: u32, uid: u32, gid: u32, ctime: i64, mtime: i64) -> Meta {
        let (atime, ctime, mtime) = self.times(ctime, mtime);
        let (uid, gid) = match self.squash_uid {
            Some(u) => (u, u),
            None => (uid, gid),
        };
        let mut perm = perm & S_IMASK;
        if kind == S_IFLNK {
            perm = 0o777;
        } else if self.squash_perms {
            perm &= !0o077;
        }
        Meta {
            perm,
            uid,
            gid,
            atime,
            ctime,
            mtime,
        }
    }
}

/// A source of tar archive bytes for one layer.
pub(crate) fn open_tar(source: &Source) -> Result<TarReader> {
    match source {
        Source::Path(p) => {
            let md = std::fs::metadata(p)
                .map_err(|e| Error::io(format!("{}: not found", p.display()), e))?;
            if !md.is_file() {
                return Err(Error::fatal(format!("{}: not a regular file", p.display())));
            }
            let f = std::fs::File::open(p).map_err(|e| Error::io(p.display().to_string(), e))?;
            TarReader::from_file(f).map_err(|e| prefix_err(&p.display().to_string(), e))
        }
        Source::Bytes(b) => TarReader::from_reader(Box::new(Cursor::new(ArcBytes(b.clone())))),
        Source::Stdin => unreachable!("standard input is read into memory first"),
    }
}

fn prefix_err(what: &str, e: Error) -> Error {
    match e {
        Error::Io { context, source } => Error::Io {
            context: format!("{what}: {context}"),
            source,
        },
        Error::Fatal(m) => Error::Fatal(format!("{what}: {m}")),
        other => other,
    }
}

struct ArcBytes(Arc<[u8]>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// An archive member's data, read up to `left` bytes.
pub(crate) struct TarData<'a> {
    pub reader: &'a mut TarReader,
    pub left: u64,
}

impl ChunkSource for TarData<'_> {
    fn chunk_size(&self, _block_size: usize) -> usize {
        1 << 20
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let want = (self.left.min(buf.len() as u64)) as usize;
        if want == 0 {
            return Ok(0);
        }
        let mut n = self.reader.read_data(&mut buf[..want])?;
        if n == 0 {
            // the stored data may end before a trailing hole
            n = self.reader.read_data(&mut buf[..want])?;
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// Reads the layers into a tree.
pub(crate) struct Scanner<'a, 'd> {
    pub tree: &'a mut Tree,
    pub log: &'a mut Log,
    pub fs: Option<&'a mut Fs<'d>>,
    pub policy: Policy,
    pub bs: u32,
    pub holes: bool,
    /// Regular files of directory layers, in the order they are written.
    pub prefetch: Vec<(PathBuf, u64)>,
    hardlinks: FxHashMap<(u64, u64), NodeId>,
}

impl<'a, 'd> Scanner<'a, 'd> {
    pub fn new(
        tree: &'a mut Tree,
        log: &'a mut Log,
        fs: Option<&'a mut Fs<'d>>,
        policy: Policy,
        bs: u32,
        holes: bool,
    ) -> Self {
        Scanner {
            tree,
            log,
            fs,
            policy,
            bs,
            holes,
            prefetch: Vec::new(),
            hardlinks: FxHashMap::default(),
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<Option<NodeId>> {
        self.tree.lookup(dir, name, self.fs.as_deref_mut())
    }

    /// The directory a layer goes into.
    pub fn target(&mut self, target: Option<&[u8]>) -> Result<NodeId> {
        let Some(t) = target else { return Ok(ROOT) };
        match self.tree.resolve_path(ROOT, t, self.fs.as_deref_mut())? {
            Some(id) if self.tree.is_dir(id) => Ok(id),
            Some(_) => Err(Error::fatal(format!(
                "target directory {}: not a directory",
                show(t)
            ))),
            None => Err(Error::fatal(format!(
                "target directory {}: not found",
                show(t)
            ))),
        }
    }

    // ---- directory layers ----

    pub fn add_dir(&mut self, src: &Path, target: NodeId, target_path: &[u8]) -> Result<()> {
        let md = std::fs::metadata(src)
            .map_err(|e| Error::io(format!("{}: not found", src.display()), e))?;
        if !md.is_dir() {
            return Err(Error::fatal(format!("{}: not a directory", src.display())));
        }
        let entries =
            read_sorted(src).map_err(|e| Error::io(format!("reading {}", src.display()), e))?;
        self.walk(entries, target, target_path)
    }

    fn walk(&mut self, entries: Vec<(Vec<u8>, PathBuf)>, dir: NodeId, prefix: &[u8]) -> Result<()> {
        for (name, path) in entries {
            let ipath = join(prefix, &name);
            if name.len() > NAME_MAX {
                self.log.warn(format!(
                    "{}: name longer than {NAME_MAX} bytes, skipped",
                    show(&ipath)
                ));
                continue;
            }
            let meta = match lstat(&path) {
                Ok(m) => m,
                Err(e) => {
                    self.log.warn(format!("{}: {e}, skipped", path.display()));
                    continue;
                }
            };
            if meta.kind == 0 {
                self.log.warn(format!(
                    "{}: unsupported file type, skipped",
                    path.display()
                ));
                continue;
            }
            if let Some(existing) = self.lookup(dir, &name)? {
                if meta.kind == S_IFDIR && self.tree.is_dir(existing) {
                    match read_sorted(&path) {
                        Ok(sub) => self.walk(sub, existing, &ipath)?,
                        Err(e) => self.log.warn(format!("{}: {e}, skipped", path.display())),
                    }
                } else {
                    self.log.warn(format!(
                        "{} already exists, keeping the existing entry",
                        show(&ipath)
                    ));
                }
                continue;
            }
            let m = self.policy.meta(
                meta.kind, meta.perm, meta.uid, meta.gid, meta.ctime, meta.mtime,
            );
            let shareable = meta.kind != S_IFDIR && meta.kind != S_IFLNK && meta.nlink > 1;
            if shareable {
                if let Some(&id) = self.hardlinks.get(&(meta.dev, meta.ino)) {
                    self.tree.link(dir, &name, id);
                    continue;
                }
            }
            let id = match meta.kind {
                S_IFDIR => {
                    let sub = match read_sorted(&path) {
                        Ok(s) => s,
                        Err(e) => {
                            self.log.warn(format!("{}: {e}, skipped", path.display()));
                            continue;
                        }
                    };
                    let id = self.tree.create(dir, &name, S_IFDIR, m, Body::Other);
                    self.walk(sub, id, &ipath)?;
                    id
                }
                S_IFREG => {
                    if !may_read(&meta) {
                        self.log
                            .warn(format!("{}: permission denied, skipped", path.display()));
                        continue;
                    }
                    let present = if self.holes {
                        match self.file_holes(&path, meta.size, &ipath) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                self.log.warn(format!("{e}, skipped"));
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    self.prefetch.push((path.clone(), meta.size));
                    let body = Body::File(Box::new(FileData {
                        size: meta.size,
                        src: if meta.size > 0 {
                            DataSrc::Path(path)
                        } else {
                            DataSrc::Empty
                        },
                        present,
                    }));
                    self.tree.create(dir, &name, S_IFREG, m, body)
                }
                S_IFLNK => {
                    let target = match host::read_link(&path) {
                        Ok(t) => t,
                        Err(e) => {
                            self.log.warn(format!("{}: {e}, skipped", path.display()));
                            continue;
                        }
                    };
                    if !self.symlink_fits(&target, &ipath) {
                        continue;
                    }
                    self.tree
                        .create(dir, &name, S_IFLNK, m, Body::Symlink(target))
                }
                S_IFCHR | S_IFBLK => self.tree.create(
                    dir,
                    &name,
                    meta.kind,
                    m,
                    Body::Device(meta.rdev_major, meta.rdev_minor),
                ),
                _ => self.tree.create(dir, &name, meta.kind, m, Body::Other),
            };
            if shareable {
                self.hardlinks.insert((meta.dev, meta.ino), id);
            }
        }
        Ok(())
    }

    fn file_holes(&mut self, path: &Path, size: u64, ipath: &[u8]) -> Result<Vec<(u64, u64)>> {
        let file = host::open(path).map_err(|e| Error::io(path.display().to_string(), e))?;
        let mut src = FileSource {
            file,
            limit: size,
            done: 0,
            name: ipath,
        };
        scan_holes(&mut src, size, self.bs)
    }

    fn symlink_fits(&mut self, target: &[u8], ipath: &[u8]) -> bool {
        if target.len() >= self.bs as usize {
            self.log.warn(format!(
                "{}: symlink target of {} bytes does not fit in a block, skipped",
                show(ipath),
                target.len()
            ));
            return false;
        }
        true
    }

    // ---- archive layers ----

    pub fn add_tar(&mut self, layer: usize, source: &Source, target: NodeId) -> Result<()> {
        let what = source.describe();
        let mut reader = open_tar(source)?;
        let mut member = 0u64;
        loop {
            let e = match reader.next_entry() {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => return Err(prefix_err(&what, e)),
            };
            self.add_member(layer, member, &e, &mut reader, target)?;
            member += 1;
        }
        Ok(())
    }

    fn add_member(
        &mut self,
        layer: usize,
        member: u64,
        e: &TarEntry,
        reader: &mut TarReader,
        target: NodeId,
    ) -> Result<()> {
        let shown = show(&e.path).into_owned();
        let base = if e.path.first() == Some(&b'/') {
            ROOT
        } else {
            target
        };
        let comps = components(&e.path);
        let ctime = e.ctime;
        let m = |p: &Policy, kind: u32| {
            p.meta(kind, e.mode, e.uid as u32, e.gid as u32, ctime, e.mtime)
        };

        // a hard link, unless the archive gives the member a type (as
        // libarchive reports old archives' links with a size: those are
        // regular files of that size)
        if let (Some(link), 0) = (&e.hardlink, e.file_type()) {
            let Some((name, parent_comps)) = comps.split_last() else {
                self.log
                    .warn(format!("{shown}: invalid hard link, skipped"));
                return Ok(());
            };
            let Some(to) = self
                .tree
                .resolve_path(target, link, self.fs.as_deref_mut())?
            else {
                return Err(Error::fatal(format!(
                    "{shown}: hard link target {} not found",
                    show(link)
                )));
            };
            if self.tree.is_dir(to) {
                self.log
                    .warn(format!("{shown}: hard link to a directory, skipped"));
                return Ok(());
            }
            let Some(parent) = self.parent(base, parent_comps, &shown)? else {
                return Ok(());
            };
            if !self.valid_name(name, &shown) {
                return Ok(());
            }
            if self.lookup(parent, name)?.is_some() {
                self.log
                    .warn(format!("{shown} already exists, hard link skipped"));
                return Ok(());
            }
            self.tree.link(parent, name, to);
            return Ok(());
        }

        let kind = e.file_type();
        if !matches!(
            kind,
            S_IFDIR | S_IFREG | S_IFLNK | S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK
        ) {
            self.log
                .warn(format!("{shown}: unsupported member type, skipped"));
            return Ok(());
        }

        // the member names an existing entry: update it
        let existing = match comps.last() {
            None => Some(base),
            Some(&last) if last == b".." => {
                self.tree.resolve(base, &comps, self.fs.as_deref_mut())?
            }
            Some(_) => None,
        };
        let (parent, name) = if let Some(x) = existing {
            (x, &b""[..])
        } else {
            let (name, parent_comps) = comps.split_last().unwrap();
            let Some(parent) = self.parent(base, parent_comps, &shown)? else {
                return Ok(());
            };
            if !self.valid_name(name, &shown) {
                return Ok(());
            }
            (parent, *name)
        };
        let existing = match existing {
            Some(x) => Some(x),
            None => self.lookup(parent, name)?,
        };
        if let Some(x) = existing {
            if self.tree.nodes[x].kind != kind {
                self.log
                    .warn(format!("{shown}: an entry of another type exists, skipped"));
                return Ok(());
            }
            let meta = m(&self.policy, kind);
            // the root's times are the filesystem time
            let times = (x != ROOT).then_some((meta.atime, meta.ctime, meta.mtime));
            self.tree.update(x, meta.perm, meta.uid, meta.gid, times);
            return Ok(());
        }
        let meta = m(&self.policy, kind);
        match kind {
            S_IFREG => {
                let size = e.size.max(0) as u64;
                let present = if self.holes {
                    let mut src = TarData { reader, left: size };
                    Some(scan_holes(&mut src, size, self.bs)?)
                } else {
                    None
                };
                let body = Body::File(Box::new(FileData {
                    size,
                    src: if size > 0 {
                        DataSrc::Tar { layer, member }
                    } else {
                        DataSrc::Empty
                    },
                    present,
                }));
                self.tree.create(parent, name, kind, meta, body);
            }
            S_IFLNK => {
                let t = e.symlink.clone().unwrap_or_default();
                let ipath = shown.as_bytes().to_vec();
                if self.symlink_fits(&t, &ipath) {
                    self.tree.create(parent, name, kind, meta, Body::Symlink(t));
                }
            }
            S_IFCHR | S_IFBLK => {
                let body = Body::Device(e.devmajor as u32, e.devminor as u32);
                self.tree.create(parent, name, kind, meta, body);
            }
            _ => {
                self.tree.create(parent, name, kind, meta, Body::Other);
            }
        }
        Ok(())
    }

    /// The directory holding a member, or None (with a warning).
    fn parent(&mut self, base: NodeId, comps: &[&[u8]], shown: &str) -> Result<Option<NodeId>> {
        match self.tree.resolve(base, comps, self.fs.as_deref_mut())? {
            Some(p) if self.tree.is_dir(p) => Ok(Some(p)),
            _ => {
                self.log
                    .warn(format!("{shown}: parent directory not found, skipped"));
                Ok(None)
            }
        }
    }

    fn valid_name(&mut self, name: &[u8], shown: &str) -> bool {
        if name.len() > NAME_MAX {
            self.log.warn(format!(
                "{shown}: name longer than {NAME_MAX} bytes, skipped"
            ));
            return false;
        }
        if name == b"." || name == b".." {
            self.log.warn(format!("{shown}: invalid name, skipped"));
            return false;
        }
        true
    }

    // ---- device tables ----

    pub fn add_devtable(&mut self, text: &[u8], times: (i64, i64), target: NodeId) -> Result<()> {
        // created entries get the table's times
        let (atime, ctime, mtime) = self.policy.times(times.0, times.1);
        for (lineno, line) in text.split(|&c| c == b'\n').enumerate() {
            let lineno = lineno + 1;
            let line = match line.iter().position(|&c| c == b'#') {
                Some(p) => &line[..p],
                None => line,
            };
            let fields: Vec<&[u8]> = line
                .split(|c| c.is_ascii_whitespace())
                .filter(|f| !f.is_empty())
                .collect();
            if fields.is_empty() {
                continue;
            }
            // numbers after the path and type; the first that is not a
            // number ends them
            let mut nums: Vec<u64> = Vec::new();
            for (i, f) in fields.iter().enumerate().skip(2).take(8) {
                let radix = if i == 2 { 8 } else { 10 };
                match std::str::from_utf8(f)
                    .ok()
                    .and_then(|s| u64::from_str_radix(s, radix).ok())
                {
                    Some(v) => nums.push(v),
                    None => break,
                }
            }
            let path = fields[0];
            if fields.len() < 3 || nums.is_empty() || fields[1].len() != 1 {
                self.log.warn(format!(
                    "device table line {lineno}: bad format for entry '{}', skipped",
                    show(path)
                ));
                continue;
            }
            let get = |i: usize, default: u64| nums.get(i).copied().unwrap_or(default);
            let mode = get(0, 0) as u32 & S_IMASK;
            let uid = get(1, 0) as u32;
            let gid = get(2, 0) as u32;
            let major = get(3, 0) as u32;
            let minor = get(4, 0);
            let start = get(5, 0);
            let inc = get(6, 1);
            let count = get(7, 0);
            let kind = match fields[1][0] {
                b'd' => S_IFDIR,
                b'f' => S_IFREG,
                b'l' => S_IFLNK,
                b'p' => S_IFIFO,
                b's' => S_IFSOCK,
                b'c' => S_IFCHR,
                b'b' => S_IFBLK,
                t => {
                    self.log.warn(format!(
                        "device table line {lineno}: unknown type '{}' for entry '{}', skipped",
                        t as char,
                        show(path)
                    ));
                    continue;
                }
            };
            let last = path.split(|&c| c == b'/').rfind(|c| !c.is_empty());
            if matches!(last, Some(b".") | Some(b"..")) {
                self.log.warn(format!(
                    "device table line {lineno}: invalid name for entry '{}', skipped",
                    show(path)
                ));
                continue;
            }
            let comps = components(path);
            if comps.is_empty() {
                if kind != S_IFDIR {
                    self.log.warn(format!(
                        "device table line {lineno}: the root must be a directory, skipped"
                    ));
                    continue;
                }
                self.tree.update(target, mode, uid, gid, None);
                continue;
            }
            let (name, parent_comps) = comps.split_last().unwrap();
            let shown = show(path).into_owned();
            let parent = match self
                .tree
                .resolve(target, parent_comps, self.fs.as_deref_mut())?
            {
                Some(p) if self.tree.is_dir(p) => p,
                _ => {
                    self.log.warn(format!(
                        "device table line {lineno}: parent directory of '{shown}' not found, skipped"
                    ));
                    continue;
                }
            };
            let meta = Meta {
                perm: if kind == S_IFLNK { 0o777 } else { mode },
                uid,
                gid,
                atime,
                ctime,
                mtime,
            };
            let entries: Vec<(Vec<u8>, u64)> = if count > 0 {
                (start..count)
                    .map(|n| {
                        let mut nm = name.to_vec();
                        nm.extend_from_slice(n.to_string().as_bytes());
                        (nm, minor.wrapping_add((n - start).wrapping_mul(inc)))
                    })
                    .collect()
            } else {
                vec![(name.to_vec(), minor)]
            };
            for (nm, minor) in entries {
                if nm.len() > NAME_MAX || nm == b".." || nm == b"." {
                    self.log.warn(format!(
                        "device table line {lineno}: invalid name for entry '{shown}', skipped"
                    ));
                    continue;
                }
                if let Some(x) = self.lookup(parent, &nm)? {
                    let perm = if self.tree.nodes[x].kind == S_IFLNK {
                        self.tree.nodes[x].perm
                    } else {
                        mode
                    };
                    self.tree.update(x, perm, uid, gid, None);
                    continue;
                }
                let body = match kind {
                    S_IFCHR | S_IFBLK => Body::Device(major, minor as u32),
                    S_IFLNK => Body::Symlink(Vec::new()),
                    S_IFREG => Body::File(Box::new(FileData {
                        size: 0,
                        src: DataSrc::Empty,
                        present: None,
                    })),
                    _ => Body::Other,
                };
                self.tree.create(parent, &nm, kind, meta, body);
            }
        }
        Ok(())
    }
}
