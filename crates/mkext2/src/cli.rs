//! The command line: xgenext2fs' options, parsed like GNU `getopt_long`.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::options::{
    Layer, LayerKind, Level, Message, Options, Readjustment, Source, Timestamps, lookup_creator_os,
    si_atof,
};
use crate::{Output, build, build_file};

/// The library and tool version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the command line asks for.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    Help,
    Version,
    Build {
        options: Options,
        /// The image path, `-` for standard output.
        output: String,
        /// `-v` also prints the version first.
        print_version: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Short(u8),
    Squash,
    Timestamp,
    Clamp,
}

struct LongOpt {
    name: &'static str,
    takes_arg: bool,
    key: Key,
}

const fn opt(name: &'static str, takes_arg: bool, key: Key) -> LongOpt {
    LongOpt {
        name,
        takes_arg,
        key,
    }
}

const LONG: &[LongOpt] = &[
    opt("starting-image", true, Key::Short(b'x')),
    opt("root", true, Key::Short(b'd')),
    opt("devtable", true, Key::Short(b'D')),
    opt("tarball", true, Key::Short(b'a')),
    opt("block-size", true, Key::Short(b'B')),
    opt("size-in-blocks", true, Key::Short(b'b')),
    opt("readjustment", true, Key::Short(b'r')),
    opt("bytes-per-inode", true, Key::Short(b'i')),
    opt("number-of-inodes", true, Key::Short(b'N')),
    opt("volume-label", true, Key::Short(b'L')),
    opt("reserved-percentage", true, Key::Short(b'm')),
    opt("creator-os", true, Key::Short(b'o')),
    opt("block-map", true, Key::Short(b'g')),
    opt("fill-value", true, Key::Short(b'e')),
    opt("allow-holes", false, Key::Short(b'z')),
    opt("faketime", false, Key::Short(b'f')),
    opt("squash", true, Key::Squash),
    opt("squash-uids", true, Key::Short(b'U')),
    opt("squash-perms", false, Key::Short(b'P')),
    opt("timestamp", true, Key::Timestamp),
    opt("clamp-timestamps", true, Key::Clamp),
    opt("help", false, Key::Short(b'h')),
    opt("version", false, Key::Short(b'V')),
    opt("verbose", false, Key::Short(b'v')),
];

/// Short options; those followed by ':' take an argument.
const SHORT: &[u8] = b"x:d:D:a:B:b:r:i:N:L:m:o:g:e:zfqU:PhVv";

fn short_takes_arg(c: u8) -> Option<bool> {
    let i = SHORT.iter().position(|&s| s == c && s != b':')?;
    Some(SHORT.get(i + 1) == Some(&b':'))
}

fn bytes(s: &OsStr) -> Vec<u8> {
    s.as_encoded_bytes().to_vec()
}

#[cfg(unix)]
fn to_os(b: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(b).to_os_string()
}

#[cfg(not(unix))]
fn to_os(b: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(b).into_owned())
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Options being collected.
struct Parsed {
    o: Options,
    timestamps: Option<(&'static str, Timestamps)>,
    print_version: bool,
}

fn integer(name: &str, v: &[u8]) -> Result<i64> {
    let s = text(v);
    s.trim()
        .parse::<i64>()
        .map_err(|_| Error::usage(format!("invalid {name} '{s}'")))
}

fn seconds(name: &str, v: &[u8]) -> Result<u32> {
    let s = text(v);
    s.trim().parse::<u32>().map_err(|_| {
        Error::usage(format!(
            "invalid {name} '{s}': expected seconds that fit in 32 bits"
        ))
    })
}

/// Split `source[:target]`, preserving a Windows drive prefix.
fn layer(kind: LayerKind, v: &[u8]) -> Result<Layer> {
    let start = if cfg!(windows) && v.len() >= 2 && v[0].is_ascii_alphabetic() && v[1] == b':' {
        2
    } else {
        0
    };
    let separator = v[start..]
        .iter()
        .position(|&c| c == b':')
        .map(|i| start + i);
    let (src, target) = match separator {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    let source = if src == b"-" {
        if kind == LayerKind::Dir {
            return Err(Error::usage(
                "a directory cannot be read from standard input",
            ));
        }
        Source::Stdin
    } else {
        if src.is_empty() {
            return Err(Error::usage("missing input path"));
        }
        Source::Path(PathBuf::from(to_os(src)))
    };
    Ok(Layer {
        kind,
        source,
        target: target.filter(|t| !t.is_empty()).map(<[u8]>::to_vec),
    })
}

impl Parsed {
    fn set_time(&mut self, flag: &'static str, t: Timestamps) -> Result<()> {
        if let Some((prev, p)) = self.timestamps {
            if p != t {
                return Err(Error::usage(format!(
                    "{prev} and {flag} cannot be combined"
                )));
            }
        }
        self.timestamps = Some((flag, t));
        Ok(())
    }

    /// Apply one option; returns a command that ends the parsing.
    fn apply(&mut self, key: Key, v: Option<&[u8]>) -> Result<Option<Command>> {
        let arg = || v.unwrap_or_default();
        let o = &mut self.o;
        match key {
            Key::Squash => {
                o.squash_uids = squash_uid(arg())?;
                o.squash_perms = true;
            }
            Key::Timestamp => {
                let t = seconds("timestamp", arg())?;
                self.set_time("--timestamp", Timestamps::Fixed(t))?;
            }
            Key::Clamp => {
                let t = seconds("clamp timestamp", arg())?;
                self.set_time("--clamp-timestamps", Timestamps::Clamp(t))?;
            }
            Key::Short(c) => match c {
                b'x' => {
                    let a = arg();
                    o.starting_image = Some(if a == b"-" {
                        Source::Stdin
                    } else {
                        Source::Path(PathBuf::from(to_os(a)))
                    });
                }
                b'd' => o.layers.push(layer(LayerKind::Dir, arg())?),
                b'D' => o.layers.push(layer(LayerKind::DevTable, arg())?),
                b'a' => o.layers.push(layer(LayerKind::Tar, arg())?),
                b'B' => {
                    o.block_size = number("block size", arg(), 0.0, f64::from(u32::MAX))? as u32;
                    o.block_size_set = true;
                }
                b'b' => {
                    o.size_in_blocks =
                        Some(number("size in blocks", arg(), 0.0, f64::from(u32::MAX))? as i64)
                }
                b'r' => o.readjustment = Readjustment::parse(&text(arg()))?,
                b'i' => {
                    let n = si_atof(&text(arg()));
                    if !(n > 0.0 && n.is_finite()) {
                        return Err(Error::usage(format!(
                            "invalid bytes per inode '{}'",
                            text(arg())
                        )));
                    }
                    o.bytes_per_inode = Some(n as f32);
                }
                b'N' => {
                    o.number_of_inodes =
                        Some(number("number of inodes", arg(), 0.0, f64::from(i32::MAX))? as i32)
                }
                b'L' => o.volume_label = Some(arg().to_vec()),
                b'm' => {
                    let m = si_atof(&text(arg()));
                    if !(0.0..100.0).contains(&m) {
                        return Err(Error::usage(format!(
                            "invalid reserved percentage '{}'",
                            text(arg())
                        )));
                    }
                    o.reserved_fraction = Some((m / 100.0) as f32);
                }
                b'o' => {
                    let os = lookup_creator_os(&text(arg()));
                    if os < 0 {
                        return Err(Error::usage(format!(
                            "invalid creator OS '{}'",
                            text(arg())
                        )));
                    }
                    o.creator_os = os;
                }
                b'g' => o.block_maps.push(arg().to_vec()),
                b'e' => o.fill_value = integer("fill value", arg())? as i32,
                b'z' => o.allow_holes = true,
                b'f' => self.set_time("--faketime", Timestamps::Zero)?,
                b'q' => {
                    o.squash_uids = Some(0);
                    o.squash_perms = true;
                }
                b'U' => o.squash_uids = squash_uid(arg())?,
                b'P' => o.squash_perms = true,
                b'h' => return Ok(Some(Command::Help)),
                b'V' => return Ok(Some(Command::Version)),
                b'v' => {
                    o.verbose = true;
                    self.print_version = true;
                }
                _ => unreachable!("unknown option key"),
            },
        }
        Ok(None)
    }
}

fn number(name: &str, v: &[u8], min: f64, max: f64) -> Result<f64> {
    let n = si_atof(&text(v));
    if !(min..=max).contains(&n) {
        return Err(Error::usage(format!("invalid {name} '{}'", text(v))));
    }
    Ok(n.floor())
}

fn squash_uid(v: &[u8]) -> Result<Option<u32>> {
    let n = integer("uid", v)?;
    match n {
        -1 => Ok(None),
        0..=0xFFFF_FFFF => Ok(Some(n as u32)),
        _ => Err(Error::usage(format!("invalid uid '{}'", text(v)))),
    }
}

/// Parse the arguments (without the program name).
pub fn parse_args(args: &[OsString]) -> Result<Command> {
    let mut p = Parsed {
        o: Options::default(),
        timestamps: None,
        print_version: false,
    };
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    let mut only_operands = false;
    while i < args.len() {
        let a = bytes(&args[i]);
        i += 1;
        if only_operands || a == b"-" || a.first() != Some(&b'-') {
            images.push(a);
            continue;
        }
        if a == b"--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = a.strip_prefix(b"--") {
            let (name, value) = match long.iter().position(|&c| c == b'=') {
                Some(k) => (&long[..k], Some(&long[k + 1..])),
                None => (long, None),
            };
            let shown = text(name);
            let exact = LONG.iter().find(|o| o.name.as_bytes() == name);
            let candidates: Vec<&LongOpt> = LONG
                .iter()
                .filter(|o| o.name.as_bytes().starts_with(name))
                .collect();
            let o = match (exact, candidates.len()) {
                (Some(o), _) => o,
                (None, 1) => candidates[0],
                (None, 0) => {
                    return Err(Error::usage(format!("unrecognized option '--{shown}'")));
                }
                (None, _) => {
                    let names: Vec<String> = candidates
                        .iter()
                        .map(|o| format!("'--{}'", o.name))
                        .collect();
                    return Err(Error::usage(format!(
                        "option '--{shown}' is ambiguous; possibilities: {}",
                        names.join(" ")
                    )));
                }
            };
            let v: Option<Vec<u8>> = if o.takes_arg {
                match value {
                    Some(v) => Some(v.to_vec()),
                    None if i < args.len() => {
                        i += 1;
                        Some(bytes(&args[i - 1]))
                    }
                    None => {
                        return Err(Error::usage(format!(
                            "option '--{}' requires an argument",
                            o.name
                        )));
                    }
                }
            } else {
                if value.is_some() {
                    return Err(Error::usage(format!(
                        "option '--{}' doesn't allow an argument",
                        o.name
                    )));
                }
                None
            };
            if let Some(c) = p.apply(o.key, v.as_deref())? {
                return Ok(c);
            }
            continue;
        }
        // a group of short options
        let mut k = 1;
        while k < a.len() {
            let c = a[k];
            k += 1;
            let Some(takes) = short_takes_arg(c) else {
                return Err(Error::usage(format!("invalid option -- '{}'", c as char)));
            };
            let v = if takes {
                if k < a.len() {
                    let v = a[k..].to_vec();
                    k = a.len();
                    Some(v)
                } else if i < args.len() {
                    i += 1;
                    Some(bytes(&args[i - 1]))
                } else {
                    return Err(Error::usage(format!(
                        "option requires an argument -- '{}'",
                        c as char
                    )));
                }
            } else {
                None
            };
            if let Some(cmd) = p.apply(Key::Short(c), v.as_deref())? {
                return Ok(cmd);
            }
        }
    }
    match images.len() {
        0 => return Err(Error::usage("missing image argument")),
        1 => {}
        _ => {
            return Err(Error::usage(format!(
                "exactly one image is expected, got {}",
                images.len()
            )));
        }
    }
    if let Some((_, t)) = p.timestamps {
        p.o.timestamps = t;
    }
    Ok(Command::Build {
        options: p.o,
        output: text(&images[0]),
        print_version: p.print_version,
    })
}

/// The usage text.
pub fn help_text(program: &str) -> String {
    format!(
        "Usage: {program} [options] image\n\
Build an ext2 filesystem image from directories, tar archives and device\n\
tables (image '-' writes to standard output).\n\
\n\
Inputs, added in order ('-' reads standard input; ':path' puts the\n\
content into that directory of the image):\n\
  -x, --starting-image <image>      add to a copy of an existing image\n\
  -d, --root <directory>[:path]     copy a directory tree\n\
  -a, --tarball <archive>[:path]    copy a tar archive (gzip, bzip2, xz, zstd)\n\
  -D, --devtable <table>[:path]     create or update nodes from a device table\n\
\n\
Size:\n\
  -B, --block-size <bytes>          1024 (default), 2048 or 4096\n\
  -b, --size-in-blocks <blocks>     image size (computed from the content otherwise)\n\
  -r, --readjustment <[+]N[unit]>   +N: free blocks to add to the computed size;\n\
                                    N: total size; units: k ki M Mi G Gi or %\n\
  -i, --bytes-per-inode <bytes>     one free inode per this much free space\n\
  -N, --number-of-inodes <count>    minimum number of inodes\n\
  -m, --reserved-percentage <pct>   blocks reserved for the superuser (default 5)\n\
\n\
Content:\n\
  -f, --faketime                    every timestamp is 0\n\
      --timestamp <seconds>         every timestamp is this value\n\
      --clamp-timestamps <seconds>  filesystem time; no file time is later\n\
  -U, --squash-uids <uid>           every file is owned by uid:uid (-1: unchanged)\n\
  -P, --squash-perms                remove group and other permissions\n\
  -q, --squash <uid>                -U <uid> -P (-q alone: uid 0)\n\
  -z, --allow-holes                 do not allocate blocks that are all zero\n\
\n\
Filesystem:\n\
  -L, --volume-label <label>        volume label (up to 16 bytes)\n\
  -o, --creator-os <os>             linux, hurd, freebsd, lites or a number\n\
  -e, --fill-value <byte>           fill unallocated blocks with this byte\n\
  -g, --block-map <path>            write the blocks of path to <path>.blk\n\
\n\
  -v, --verbose                     describe the image built\n\
  -h, --help                        show this help\n\
  -V, --version                     show the version\n\
\n\
Without a timestamp option the filesystem time is $SOURCE_DATE_EPOCH (or\n\
the current time) and file times come from the inputs.\n"
    )
}

/// Run the command line tool; returns the exit status.
pub fn main_with_args(args: impl IntoIterator<Item = OsString>) -> i32 {
    let mut it = args.into_iter();
    let program = it
        .next()
        .map(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "mkext2".to_string())
        })
        .unwrap_or_else(|| "mkext2".to_string());
    let rest: Vec<OsString> = it.collect();
    match parse_args(&rest) {
        Err(e) => {
            eprintln!("{program}: {e}");
            eprintln!("Try '{program} --help' for more information.");
            1
        }
        Ok(Command::Help) => {
            eprint!("{}", help_text(&program));
            0
        }
        Ok(Command::Version) => {
            println!("mkext2 {VERSION}");
            0
        }
        Ok(Command::Build {
            mut options,
            output,
            print_version,
        }) => {
            let to_stdout = output == "-";
            if print_version {
                if to_stdout {
                    eprintln!("mkext2 {VERSION}");
                } else {
                    println!("mkext2 {VERSION}");
                }
            }
            let prog = program.clone();
            options.message_sink = Some(Arc::new(move |m: &Message| match m.level {
                Level::Info => eprintln!("{}", m.text),
                Level::Warning => eprintln!("{prog}: {}", m.text),
            }));
            let result = if to_stdout {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                build(&options, Output::Writer(&mut lock))
            } else {
                build_file(&options, &output)
            };
            match result {
                Ok(report) => {
                    if let Some(v) = report.verbose_output {
                        if to_stdout {
                            eprint!("{v}");
                        } else {
                            print!("{v}");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    0
                }
                Err(e) => {
                    eprintln!("{program}: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> Result<Command> {
        let v: Vec<OsString> = a.iter().map(OsString::from).collect();
        parse_args(&v)
    }

    fn opts(a: &[&str]) -> (Options, String) {
        match parse(a).unwrap() {
            Command::Build {
                options, output, ..
            } => (options, output),
            _ => panic!("not a build"),
        }
    }

    #[test]
    fn getopt_forms() {
        let (o, out) = opts(&["-fzP", "-r+500", "-B4096", "-d", "dir", "img"]);
        assert_eq!(out, "img");
        assert!(o.allow_holes && o.squash_perms);
        assert_eq!(o.block_size, 4096);
        assert_eq!(o.timestamps, Timestamps::Zero);
        assert_eq!(o.readjustment.as_str(), "+500");
        let (o, _) = opts(&[
            "--fake",
            "--block-s=2048",
            "--read=+500",
            "--ro",
            "d",
            "img",
        ]);
        assert_eq!(o.block_size, 2048);
        assert_eq!(o.layers.len(), 1);
        let (o, out) = opts(&["-d", "dir:/opt", "--", "-x"]);
        assert_eq!(out, "-x");
        assert_eq!(o.layers[0].target.as_deref(), Some(&b"/opt"[..]));
        assert!(parse(&["img", "--", "-x"]).is_err());
        let (o, _) = opts(&["--squash", "7", "img"]);
        assert_eq!(o.squash_uids, Some(7));
        let (o, _) = opts(&["-q", "img"]);
        assert_eq!(o.squash_uids, Some(0));
        let (o, _) = opts(&["-U", "-1", "img"]);
        assert_eq!(o.squash_uids, None);
        let (o, out) = opts(&["-a", "-:/x", "-"]);
        assert_eq!(out, "-");
        assert!(matches!(o.layers[0].source, Source::Stdin));
        assert_eq!(o.layers[0].target.as_deref(), Some(&b"/x"[..]));
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_paths_and_targets() {
        for (flag, input, source, target) in [
            ("-d", r"C:\root", r"C:\root", None),
            ("-a", "C:/root.tar:/opt", "C:/root.tar", Some("/opt")),
            ("-D", "C:devices.txt:/dev", "C:devices.txt", Some("/dev")),
        ] {
            let (options, _) = opts(&[flag, input, "out.ext2"]);
            let layer = &options.layers[0];
            assert!(matches!(&layer.source, Source::Path(p) if p == &PathBuf::from(source)));
            assert_eq!(layer.target.as_deref(), target.map(str::as_bytes));
        }
    }

    #[test]
    fn errors() {
        for a in [
            &["--squ", "1", "x"][..],
            &["--bogus", "x"],
            &["-B"],
            &[],
            &["a", "b"],
            &["--timestamp", "yesterday", "x"],
            &["--faketime=1", "x"],
            &["-m", "-1", "x"],
            &["-r", "+12x", "x"],
            &["-f", "--timestamp", "5", "x"],
        ] {
            assert!(parse(a).is_err(), "{a:?}");
        }
        assert!(matches!(parse(&["-h", "--bogus"]), Ok(Command::Help)));
        assert!(matches!(parse(&["-V"]), Ok(Command::Version)));
    }
}
