//! Shared helpers for integration tests.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Deterministic xorshift PRNG.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

/// The reference implementation (xgenext2fs 1.5.6). It is required: set
/// `XGENEXT2FS` to its path when it is not on the `PATH`.
pub fn reference() -> Option<PathBuf> {
    let bin = std::env::var("XGENEXT2FS").unwrap_or_else(|_| "xgenext2fs".to_string());
    match Command::new(&bin).arg("--version").output() {
        Ok(out) if out.status.success() => Some(PathBuf::from(bin)),
        _ => panic!(
            "xgenext2fs (the reference implementation) is required to run the tests.\n\
             Linux: install the .deb from https://github.com/cartesi/genext2fs/releases/tag/v1.5.6\n\
             macOS: brew install cartesi/tap/xgenext2fs\n\
             or set XGENEXT2FS to its path."
        ),
    }
}

/// Our binary.
pub fn ours() -> PathBuf {
    PathBuf::from(option_env!("CARGO_BIN_EXE_mkext2").unwrap_or("mkext2"))
}

pub fn run(bin: &Path, args: &[&OsStr], cwd: &Path) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("SOURCE_DATE_EPOCH", "1700000000")
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", bin.display()))
}

pub fn is_root() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::geteuid() == 0
    }
    #[cfg(not(unix))]
    false
}

#[cfg(unix)]
pub fn mkfifo(p: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    unsafe {
        libc::mkfifo(c.as_ptr(), 0o644);
    }
}

#[cfg(unix)]
pub fn mkdev(p: &Path, char: bool, major: u32, minor: u32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let kind = if char { libc::S_IFCHR } else { libc::S_IFBLK };
    #[cfg(target_os = "linux")]
    let dev = libc::makedev(major, minor);
    #[cfg(not(target_os = "linux"))]
    let dev = libc::makedev(major as i32, minor as i32);
    unsafe { libc::mknod(c.as_ptr(), kind | 0o640, dev) == 0 }
}

#[cfg(unix)]
pub fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
pub fn mksocket(p: &Path) {
    let _ = std::os::unix::net::UnixListener::bind(p);
}

/// Content for a file of `size` bytes, sometimes with all-zero regions
/// (so that `-z` has holes to create).
pub fn content(rng: &mut Rng, size: usize) -> Vec<u8> {
    let mut v = rng.bytes(size);
    if size > 4096 && rng.chance(40) {
        // zero out a few aligned and unaligned regions
        for _ in 0..rng.range(1, 4) {
            let start = rng.below(size as u64) as usize;
            let len = (rng.range(1024, 20000) as usize).min(size - start);
            v[start..start + len].fill(0);
        }
    }
    if size > 0 && rng.chance(5) {
        v.fill(0);
    }
    v
}

/// Size profile for generated files.
#[derive(Clone, Copy)]
pub struct Profile {
    pub max_depth: u32,
    pub max_entries: u64,
    /// Largest regular file generated.
    pub max_file: usize,
    pub specials: bool,
    /// Longest symlink target generated.
    pub max_symlink: usize,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            max_depth: 3,
            max_entries: 12,
            max_file: 400_000,
            specials: true,
            max_symlink: 4095,
        }
    }
}

const NAME_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-+ ~,";

pub fn name(rng: &mut Rng) -> String {
    let len = match rng.below(20) {
        0 => rng.range(200, 255),
        1 => 1,
        2..=4 => rng.range(30, 80),
        _ => rng.range(2, 16),
    } as usize;
    let mut s = String::new();
    if rng.chance(5) {
        s.push_str(rng.pick(&["ünïcödé-", "日本語-", "emoji-😀-", "Ω-"]));
    }
    while s.len() < len {
        s.push(*rng.pick(NAME_CHARS) as char);
    }
    // keep within 255 bytes, on a char boundary
    while s.len() > 255 {
        s.pop();
    }
    if s == "." || s == ".." {
        s.push('x');
    }
    s
}

fn file_size(rng: &mut Rng, max: usize) -> usize {
    let choices: [usize; 14] = [
        0,
        1,
        59,
        1023,
        1024,
        1025,
        4096,
        12 * 1024,
        12 * 1024 + 1,
        13 * 4096,
        (12 + 256) * 1024,
        (12 + 256) * 1024 + 1,
        (12 + 512) * 2048 + 7,
        max,
    ];
    let s = match rng.below(4) {
        0 => *rng.pick(&choices),
        1 => rng.below(4096) as usize,
        2 => rng.below(64 * 1024) as usize,
        _ => rng.below(max as u64 + 1) as usize,
    };
    s.min(max)
}

/// Populate `root` with a random tree. Returns the list of regular files
/// created (for hard link targets).
pub fn gen_tree(root: &Path, seed: u64, profile: Profile) {
    let mut rng = Rng::new(seed);
    let mut files: Vec<PathBuf> = Vec::new();
    fs::create_dir_all(root).unwrap();
    gen_dir(root, &mut rng, profile, 0, &mut files);
}

fn gen_dir(dir: &Path, rng: &mut Rng, p: Profile, depth: u32, files: &mut Vec<PathBuf>) {
    let n = rng.range(0, p.max_entries);
    for _ in 0..n {
        let nm = name(rng);
        let path = dir.join(&nm);
        if path.exists() || path.symlink_metadata().is_ok() {
            continue;
        }
        let kind = rng.below(100);
        match kind {
            0..=49 => {
                let size = file_size(rng, p.max_file);
                let data = content(rng, size);
                fs::write(&path, &data).unwrap();
                #[cfg(unix)]
                if rng.chance(30) {
                    let mode = *rng.pick(&[
                        0o600, 0o644, 0o755, 0o700, 0o444, 0o4755, 0o2755, 0o1777, 0o640,
                    ]);
                    chmod(&path, mode);
                }
                files.push(path);
            }
            50..=64 if depth < p.max_depth => {
                fs::create_dir(&path).unwrap();
                gen_dir(&path, rng, p, depth + 1, files);
                #[cfg(unix)]
                if rng.chance(20) {
                    chmod(&path, *rng.pick(&[0o700, 0o755, 0o775, 0o1777]));
                }
            }
            65..=79 => {
                let len = (*rng.pick(&[
                    1usize, 10, 58, 59, 60, 61, 100, 1023, 1024, 1025, 2000, 4095,
                ]))
                .min(p.max_symlink);
                let target: String = (0..len).map(|_| *rng.pick(b"abc/._-xyz") as char).collect();
                // (the length limit varies across systems; skip when refused)
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&target, &path);
                #[cfg(not(unix))]
                let _ = target;
            }
            80..=89 => {
                if !files.is_empty() {
                    let src = rng.pick(files).clone();
                    let _ = fs::hard_link(&src, &path);
                }
            }
            90..=94 if p.specials => {
                #[cfg(unix)]
                mkfifo(&path);
            }
            95..=96 if p.specials => {
                #[cfg(unix)]
                mksocket(&path);
            }
            #[cfg(unix)]
            97..=99 if p.specials && is_root() => {
                let ch = rng.chance(50);
                mkdev(&path, ch, rng.below(300) as u32, rng.below(300) as u32);
            }
            _ => {}
        }
    }
}

/// A big directory exercising directory block packing (first-fit reuse
/// of the slack at the end of earlier blocks).
pub fn gen_wide_dir(dir: &Path, rng: &mut Rng, count: usize) {
    fs::create_dir_all(dir).unwrap();
    for i in 0..count {
        let len = rng.range(1, 120) as usize;
        let mut nm = format!("{i:05}");
        while nm.len() < len {
            nm.push(*rng.pick(b"abcdefgh") as char);
        }
        let mut f = fs::File::create(dir.join(nm)).unwrap();
        let n = rng.below(100) as usize;
        f.write_all(&rng.bytes(n)).unwrap();
    }
}

pub struct TempDir(tempfile::TempDir);

impl TempDir {
    pub fn new() -> Self {
        TempDir(
            tempfile::Builder::new()
                .prefix("mkext2-test")
                .tempdir()
                .unwrap(),
        )
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// Run the reference and our tool with the same arguments (the image name
/// is appended). Our image must be valid and, when the reference succeeds,
/// hold the same content (see `reader::compare_content`); block map files
/// must exist for the same paths and report the same sizes.
pub fn compare(work: &Path, args: &[&str]) -> Result<CompareOutcome, String> {
    let Some(reference) = reference() else {
        return Ok(CompareOutcome::Skipped);
    };
    let ref_dir = work.join("ref-run");
    let new_dir = work.join("new-run");
    let _ = fs::remove_dir_all(&ref_dir);
    let _ = fs::remove_dir_all(&new_dir);
    fs::create_dir_all(&ref_dir).unwrap();
    fs::create_dir_all(&new_dir).unwrap();
    let mut a: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    a.push(OsStr::new("image.img"));
    let r = run(&reference, &a, &ref_dir);
    let n = run(&ours(), &a, &new_dir);
    let rok = r.status.success();
    let nok = n.status.success();
    let stderr = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
    if !nok {
        if rok {
            return Err(format!(
                "mkext2 failed where xgenext2fs succeeded for {args:?}: {}",
                stderr(&n)
            ));
        }
        return Ok(CompareOutcome::BothFailed(stderr(&r), stderr(&n)));
    }
    let ni = fs::read(new_dir.join("image.img")).unwrap();
    let ours_img = mkext2::reader::Image::new(ni.clone())
        .map_err(|e| format!("{args:?}: unreadable image: {e}"))?;
    let problems = ours_img.check().map_err(|e| e.to_string())?;
    if !problems.is_empty() {
        return Err(format!("{args:?}: invalid image: {problems:#?}"));
    }
    if !rok {
        // xgenext2fs fails in some cases mkext2 handles (for example when
        // its size estimate is too small); the image only has to be valid
        return Ok(CompareOutcome::ReferenceFailed(ni));
    }
    let ri = fs::read(ref_dir.join("image.img")).unwrap();
    let ref_img = mkext2::reader::Image::new(ri).map_err(|e| e.to_string())?;
    let opts = mkext2::reader::CompareOptions {
        device_low_bits: true,
        ..Default::default()
    };
    let diffs =
        mkext2::reader::compare_content(&ref_img, &ours_img, &opts).map_err(|e| e.to_string())?;
    if !diffs.is_empty() {
        return Err(format!(
            "{args:?}: content differs from xgenext2fs: {diffs:#?}"
        ));
    }
    // block maps: same files, same sizes (the block numbers are layout)
    for ent in fs::read_dir(&ref_dir).unwrap() {
        let ent = ent.unwrap();
        let nm = ent.file_name();
        if nm.to_string_lossy().ends_with(".blk") {
            let a = fs::read_to_string(ent.path()).unwrap();
            let b = fs::read_to_string(new_dir.join(&nm))
                .map_err(|e| format!("missing block map {nm:?}: {e}"))?;
            let size = |s: &str| s.split(':').next().map(str::to_string);
            if size(&a) != size(&b) {
                return Err(format!(
                    "block map {nm:?}: size {:?} vs {:?}",
                    size(&a),
                    size(&b)
                ));
            }
        }
    }
    Ok(CompareOutcome::Equivalent(ni))
}

pub enum CompareOutcome {
    Skipped,
    Equivalent(Vec<u8>),
    ReferenceFailed(Vec<u8>),
    BothFailed(String, String),
}

/// Assert that our image is valid and equivalent to xgenext2fs' (when that
/// succeeds); returns our image when one was produced.
pub fn assert_equivalent(work: &Path, args: &[&str]) -> Option<Vec<u8>> {
    match compare(work, args) {
        Ok(CompareOutcome::Equivalent(img)) | Ok(CompareOutcome::ReferenceFailed(img)) => Some(img),
        Ok(CompareOutcome::BothFailed(..)) | Ok(CompareOutcome::Skipped) => None,
        Err(e) => panic!("{e}"),
    }
}

/// Run e2fsck on an image when available.
pub fn e2fsck(img: &Path) -> Option<Result<(), String>> {
    let bin = std::env::var("E2FSCK").unwrap_or_else(|_| "e2fsck".to_string());
    let out = Command::new(&bin).args(["-fn"]).arg(img).output().ok()?;
    if out.status.success() {
        Some(Ok(()))
    } else {
        Some(Err(format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}
