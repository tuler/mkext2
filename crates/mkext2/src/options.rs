//! Public description of an image to build, and of the build's outcome.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};

/// Where an input comes from.
#[derive(Clone, Debug)]
pub enum Source {
    Path(PathBuf),
    /// Standard input (read into memory).
    Stdin,
    Bytes(Arc<[u8]>),
}

impl Source {
    /// A short description for messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Source::Path(p) => p.display().to_string(),
            Source::Stdin => "standard input".to_string(),
            Source::Bytes(_) => "memory".to_string(),
        }
    }
}

/// What a layer contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerKind {
    /// A directory tree.
    Dir,
    /// A tar archive, possibly compressed.
    Tar,
    /// A device table (mkfs.jffs2 format).
    DevTable,
}

/// One input, added to the image in order.
#[derive(Clone, Debug)]
pub struct Layer {
    pub kind: LayerKind,
    pub source: Source,
    /// Directory of the image the content goes into (default `/`).
    pub target: Option<Vec<u8>>,
}

impl Layer {
    pub fn dir(path: impl AsRef<Path>) -> Self {
        Layer {
            kind: LayerKind::Dir,
            source: Source::Path(path.as_ref().to_path_buf()),
            target: None,
        }
    }

    pub fn tar(path: impl AsRef<Path>) -> Self {
        Layer {
            kind: LayerKind::Tar,
            source: Source::Path(path.as_ref().to_path_buf()),
            target: None,
        }
    }

    pub fn tar_bytes(data: impl Into<Arc<[u8]>>) -> Self {
        Layer {
            kind: LayerKind::Tar,
            source: Source::Bytes(data.into()),
            target: None,
        }
    }

    pub fn devtable(path: impl AsRef<Path>) -> Self {
        Layer {
            kind: LayerKind::DevTable,
            source: Source::Path(path.as_ref().to_path_buf()),
            target: None,
        }
    }

    pub fn devtable_bytes(data: impl Into<Arc<[u8]>>) -> Self {
        Layer {
            kind: LayerKind::DevTable,
            source: Source::Bytes(data.into()),
            target: None,
        }
    }

    /// Put the content into this directory of the image.
    pub fn at(mut self, target: impl AsRef<[u8]>) -> Self {
        self.target = Some(target.as_ref().to_vec());
        self
    }
}

/// Timestamp policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Timestamps {
    /// Filesystem time from `SOURCE_DATE_EPOCH` (or now); file times from
    /// the inputs.
    #[default]
    Source,
    /// Every time is 0 (`--faketime`).
    Zero,
    /// Every time, filesystem and files, is this value.
    Fixed(u32),
    /// The filesystem time is this value, file times are at most this value.
    Clamp(u32),
}

/// The size readjustment (`-r`): `[+]N[suffix]` or `[+]N%`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readjustment(String);

/// A parsed readjustment, see [`Readjustment::apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readjusted {
    /// Keep at least this many free blocks besides the reserve.
    Extra(u64),
    /// The image has at least this many blocks.
    Size(u64),
}

impl Default for Readjustment {
    fn default() -> Self {
        Readjustment("+0".to_string())
    }
}

impl Readjustment {
    /// Store a readjustment without checking it (see [`Readjustment::parse`]).
    pub fn new(s: impl Into<String>) -> Self {
        Readjustment(s.into())
    }

    /// Parse and check a readjustment.
    pub fn parse(s: &str) -> Result<Self> {
        let r = Readjustment(s.to_string());
        r.apply(1)?;
        Ok(r)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Interpret the readjustment for a content needing `needed` blocks.
    pub fn apply(&self, needed: u64) -> Result<Readjusted> {
        let bad = || Error::usage(format!("invalid readjustment '{}'", self.0));
        let s = self.0.trim();
        let (relative, s) = match s.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let end = s
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(s.len());
        let (num, suffix) = s.split_at(end);
        if num.is_empty() || num == "." || num.matches('.').count() > 1 {
            return Err(bad());
        }
        let n: f64 = num.parse().map_err(|_| bad())?;
        let value = match suffix {
            "%" => (needed as f64 * n / 100.0).floor(),
            "" => n,
            "k" => n * 1e3,
            "ki" => n * 1024.0,
            "M" => n * 1e6,
            "Mi" => n * 1048576.0,
            "G" => n * 1e9,
            "Gi" => n * 1073741824.0,
            _ => return Err(bad()),
        };
        if !value.is_finite() || value > 1e15 {
            return Err(bad());
        }
        let value = value.floor() as u64;
        Ok(if relative {
            Readjusted::Extra(value)
        } else {
            Readjusted::Size(value)
        })
    }
}

/// Severity of a [`Message`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Progress and informational messages.
    Info,
    /// Something was left out or changed.
    Warning,
}

/// A diagnostic produced while building.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub level: Level,
    pub text: String,
}

/// Receives messages as they are produced.
pub type MessageSink = Arc<dyn Fn(&Message) + Send + Sync>;

/// Description of the image to build.
#[derive(Clone)]
pub struct Options {
    /// 1024, 2048 or 4096.
    pub block_size: u32,
    /// Whether the block size was given (it must then agree with `-x`).
    pub block_size_set: bool,
    /// Image size in blocks (`-b`); computed from the content when absent.
    pub size_in_blocks: Option<i64>,
    pub readjustment: Readjustment,
    /// At least one free inode per this many bytes of free space (`-i`).
    pub bytes_per_inode: Option<f32>,
    /// Minimum number of inodes (`-N`).
    pub number_of_inodes: Option<i32>,
    pub volume_label: Option<Vec<u8>>,
    /// Reserved fraction of the blocks (`-m` / 100; default 0.05).
    pub reserved_fraction: Option<f32>,
    pub creator_os: i32,
    /// Byte written to unallocated blocks (`-e`; 0 leaves them zero).
    pub fill_value: i32,
    /// Do not allocate blocks that are entirely zero (`-z`).
    pub allow_holes: bool,
    pub timestamps: Timestamps,
    /// Filesystem time with [`Timestamps::Source`], instead of the
    /// `SOURCE_DATE_EPOCH` environment variable.
    pub source_date_epoch: Option<i64>,
    /// Every owner becomes `uid:uid` (`-U`).
    pub squash_uids: Option<u32>,
    /// Clear group and other permissions (`-P`).
    pub squash_perms: bool,
    /// Start from this image (`-x`).
    pub starting_image: Option<Source>,
    /// Paths to write block maps for (`-g`).
    pub block_maps: Vec<Vec<u8>>,
    /// Directory the block maps are written to (default: current directory).
    pub block_map_dir: Option<PathBuf>,
    /// Describe the result (`-v`), in [`Report::verbose_output`].
    pub verbose: bool,
    pub layers: Vec<Layer>,
    pub message_sink: Option<MessageSink>,
    /// Read-ahead threads (never changes the output).
    pub threads: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            block_size: 1024,
            block_size_set: false,
            size_in_blocks: None,
            readjustment: Readjustment::default(),
            bytes_per_inode: None,
            number_of_inodes: None,
            volume_label: None,
            reserved_fraction: None,
            creator_os: 0,
            fill_value: 0,
            allow_holes: false,
            timestamps: Timestamps::Source,
            source_date_epoch: None,
            squash_uids: None,
            squash_perms: false,
            starting_image: None,
            block_maps: Vec::new(),
            block_map_dir: None,
            verbose: false,
            layers: Vec::new(),
            message_sink: None,
            threads: None,
        }
    }
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("block_size", &self.block_size)
            .field("size_in_blocks", &self.size_in_blocks)
            .field("readjustment", &self.readjustment)
            .field("timestamps", &self.timestamps)
            .field("layers", &self.layers)
            .finish_non_exhaustive()
    }
}

/// Figures of the built image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageInfo {
    pub block_size: u32,
    pub blocks: u32,
    pub free_blocks: u32,
    pub reserved_blocks: u32,
    pub inodes: u32,
    pub free_inodes: u32,
    pub groups: u32,
}

/// Outcome of a build.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub info: ImageInfo,
    pub messages: Vec<Message>,
    /// The description requested with [`Options::verbose`].
    pub verbose_output: Option<String>,
}

/// Parse a number like C's `atof`, followed by an optional multiplier:
/// `k`, `M`, `G` (powers of 1000) or `Ki`, `Mi`, `Gi` (powers of 1024).
/// Other trailing text is ignored; no number at all gives 0.
pub fn si_atof(s: &str) -> f64 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || &s[digits_start..i] == "." {
        return 0.0;
    }
    let n: f64 = s[..i].parse().unwrap_or(0.0);
    let rest = &s[i..];
    let mult = if rest.starts_with("Ki") {
        1024.0
    } else if rest.starts_with("Mi") {
        1048576.0
    } else if rest.starts_with("Gi") {
        1073741824.0
    } else if rest.starts_with('k') {
        1e3
    } else if rest.starts_with('M') {
        1e6
    } else if rest.starts_with('G') {
        1e9
    } else {
        1.0
    };
    n * mult
}

/// The creator OS value for a name (`linux`, `hurd` or `gnu`, `freebsd`,
/// `lites`, case-insensitive) or a number. Unknown names mean Linux (0).
pub fn lookup_creator_os(name: &str) -> i32 {
    let t = name.trim();
    match t.to_ascii_lowercase().as_str() {
        "linux" => return 0,
        "hurd" | "gnu" => return 1,
        "masix" => return 2,
        "freebsd" => return 3,
        "lites" => return 4,
        _ => {}
    }
    let b = t.as_bytes();
    let mut i = usize::from(!b.is_empty() && (b[0] == b'-' || b[0] == b'+'));
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return 0;
    }
    t[..i]
        .parse::<i64>()
        .map(|v| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(si_atof("8k"), 8000.0);
        assert_eq!(si_atof("1Ki"), 1024.0);
        assert_eq!(si_atof("1Mi"), 1048576.0);
        assert_eq!(si_atof("12.5"), 12.5);
        assert_eq!(si_atof("4096xyz"), 4096.0);
        assert_eq!(si_atof("x"), 0.0);
        assert_eq!(si_atof("-1"), -1.0);
    }

    #[test]
    fn readjustments() {
        let r = |s: &str, n| Readjustment::parse(s).unwrap().apply(n).unwrap();
        assert_eq!(r("+500", 0), Readjusted::Extra(500));
        assert_eq!(r("+30%", 1000), Readjusted::Extra(300));
        assert_eq!(r("+1k", 0), Readjusted::Extra(1000));
        assert_eq!(r("+2ki", 0), Readjusted::Extra(2048));
        assert_eq!(r("150%", 1000), Readjusted::Size(1500));
        assert_eq!(r("3Mi", 0), Readjusted::Size(3 << 20));
        for bad in ["+12x", "", "+", "abc", "+1.2.3", "5K"] {
            let e = Readjustment::parse(bad).unwrap_err().to_string();
            assert!(e.contains("readjustment"), "{bad}: {e}");
        }
    }

    #[test]
    fn creator_os() {
        assert_eq!(lookup_creator_os("FreeBSD"), 3);
        assert_eq!(lookup_creator_os("hurd"), 1);
        assert_eq!(lookup_creator_os("5"), 5);
        assert_eq!(lookup_creator_os("bogus"), 0);
        assert_eq!(lookup_creator_os("-2"), -2);
    }
}
