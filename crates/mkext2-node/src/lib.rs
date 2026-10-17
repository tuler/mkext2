//! Node.js binding for mkext2.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mkext2::cli::{Command, parse_args};
use napi::bindgen_prelude::*;
use napi_derive::napi;

/// One input added to the image, in order.
#[napi(object)]
pub struct LayerSpec {
    /// `"dir"`, `"tar"` or `"devtable"`.
    #[napi(js_name = "type")]
    pub kind: String,
    /// Path of the directory, archive or device table.
    pub path: Option<String>,
    /// In-memory archive or device table (instead of `path`).
    pub data: Option<Buffer>,
    /// Directory inside the image to add the content to (default `/`).
    pub target: Option<String>,
}

/// Image description; mirrors the command line options.
#[napi(object)]
#[derive(Default)]
pub struct BuildOptions {
    /// 1024 (default), 2048 or 4096 (`-B`).
    pub block_size: Option<u32>,
    /// Image size in blocks (`-b`); computed from the inputs when absent.
    pub size_in_blocks: Option<i64>,
    /// Extra blocks and inodes over the computed size (`-r`), e.g. `"+1000"` or `"+10%"`.
    pub readjustment: Option<String>,
    /// Bytes per inode (`-i`); only effective with `sizeInBlocks`.
    pub bytes_per_inode: Option<f64>,
    /// Minimum number of inodes (`-N`).
    pub number_of_inodes: Option<u32>,
    /// Volume label (`-L`).
    pub volume_label: Option<String>,
    /// Percentage of reserved blocks (`-m`, default 5).
    pub reserved_percentage: Option<f64>,
    /// Creator OS (`-o`): `"linux"`, `"hurd"`, `"freebsd"` or a number.
    pub creator_os: Option<String>,
    /// Fill unallocated blocks with this byte (`-e`).
    pub fill_value: Option<u32>,
    /// Do not allocate all-zero blocks (`-z`).
    pub allow_holes: Option<bool>,
    /// All timestamps are 0 (`-f`).
    pub faketime: Option<bool>,
    /// All timestamps (filesystem and files) are this value.
    pub timestamp: Option<u32>,
    /// Filesystem time is this value; file times are clamped to it.
    pub clamp_timestamps: Option<u32>,
    /// Filesystem time when no other timestamp option is given (defaults
    /// to the `SOURCE_DATE_EPOCH` environment variable, then now).
    pub source_date_epoch: Option<i64>,
    /// Make every file owned by this uid:gid (`-U`).
    pub squash_uids: Option<u32>,
    /// Remove group and other permissions (`-P`).
    pub squash_perms: Option<bool>,
    /// Existing image to add to (`-x`).
    pub starting_image: Option<String>,
    /// Paths to write `.blk` block maps for (`-g`).
    pub block_maps: Option<Vec<String>>,
    /// Directory for block map files (default: current directory).
    pub block_map_dir: Option<String>,
    /// Content layers, applied in order.
    pub layers: Option<Vec<LayerSpec>>,
    /// Read-ahead threads (default: up to 4; 0 or 1 disables).
    pub threads: Option<u32>,
}

/// A diagnostic message.
#[napi(object)]
pub struct Message {
    /// `"info"` or `"warning"`.
    pub level: String,
    pub text: String,
}

/// Summary of a built image.
#[napi(object)]
pub struct BuildResult {
    pub block_size: u32,
    pub blocks: u32,
    pub free_blocks: u32,
    pub reserved_blocks: u32,
    pub inodes: u32,
    pub free_inodes: u32,
    pub groups: u32,
    pub messages: Vec<Message>,
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}

fn to_options(o: BuildOptions) -> Result<mkext2::Options> {
    let mut opts = mkext2::Options::default();
    if let Some(bs) = o.block_size {
        opts.block_size = bs;
        opts.block_size_set = true;
    }
    opts.size_in_blocks = o.size_in_blocks;
    if let Some(r) = o.readjustment {
        opts.readjustment = mkext2::Readjustment::parse(&r).map_err(err)?;
    }
    opts.bytes_per_inode = o.bytes_per_inode.map(|v| v as f32);
    opts.number_of_inodes = o
        .number_of_inodes
        .map(|n| i32::try_from(n).map_err(|_| err("numberOfInodes out of range")))
        .transpose()?;
    opts.volume_label = o.volume_label.map(String::into_bytes);
    opts.reserved_fraction = o.reserved_percentage.map(|p| p as f32 / 100.0);
    if let Some(os) = o.creator_os {
        opts.creator_os = mkext2::lookup_creator_os(&os);
    }
    if let Some(v) = o.fill_value {
        if v > 255 {
            return Err(err("fillValue must be a byte value (0-255)"));
        }
        opts.fill_value = v as i32;
    }
    opts.allow_holes = o.allow_holes.unwrap_or(false);
    let exclusive = [
        o.faketime == Some(true),
        o.timestamp.is_some(),
        o.clamp_timestamps.is_some(),
    ];
    if exclusive.iter().filter(|&&b| b).count() > 1 {
        return Err(err("faketime, timestamp and clampTimestamps are exclusive"));
    }
    opts.timestamps = if o.faketime == Some(true) {
        mkext2::Timestamps::Zero
    } else if let Some(t) = o.timestamp {
        mkext2::Timestamps::Fixed(t)
    } else if let Some(t) = o.clamp_timestamps {
        mkext2::Timestamps::Clamp(t)
    } else {
        mkext2::Timestamps::Source
    };
    opts.source_date_epoch = o.source_date_epoch;
    opts.squash_uids = o.squash_uids;
    opts.squash_perms = o.squash_perms.unwrap_or(false);
    opts.starting_image = o
        .starting_image
        .map(|p| mkext2::Source::Path(PathBuf::from(p)));
    opts.block_maps = o
        .block_maps
        .unwrap_or_default()
        .into_iter()
        .map(String::into_bytes)
        .collect();
    opts.block_map_dir = o.block_map_dir.map(PathBuf::from);
    opts.threads = o.threads.map(|t| t as usize);
    for l in o.layers.unwrap_or_default() {
        let kind = match l.kind.as_str() {
            "dir" | "directory" => mkext2::LayerKind::Dir,
            "tar" | "tarball" => mkext2::LayerKind::Tar,
            "devtable" => mkext2::LayerKind::DevTable,
            other => return Err(err(format!("unknown layer type '{other}'"))),
        };
        let source = match (l.path, l.data) {
            (Some(p), None) => mkext2::Source::Path(PathBuf::from(p)),
            (None, Some(d)) => {
                if kind == mkext2::LayerKind::Dir {
                    return Err(err("directory layers need a path"));
                }
                mkext2::Source::Bytes(d.to_vec().into())
            }
            _ => return Err(err("a layer needs exactly one of path or data")),
        };
        opts.layers.push(mkext2::Layer {
            kind,
            source,
            target: l.target.map(String::into_bytes),
        });
    }
    Ok(opts)
}

fn to_result(report: mkext2::Report) -> BuildResult {
    let i = report.info;
    BuildResult {
        block_size: i.block_size,
        blocks: i.blocks,
        free_blocks: i.free_blocks,
        reserved_blocks: i.reserved_blocks,
        inodes: i.inodes,
        free_inodes: i.free_inodes,
        groups: i.groups,
        messages: report
            .messages
            .into_iter()
            .map(|m| Message {
                level: match m.level {
                    mkext2::Level::Info => "info".to_string(),
                    mkext2::Level::Warning => "warning".to_string(),
                },
                text: m.text,
            })
            .collect(),
    }
}

pub struct BuildFileTask {
    options: mkext2::Options,
    output: PathBuf,
}

impl Task for BuildFileTask {
    type Output = mkext2::Report;
    type JsValue = BuildResult;

    fn compute(&mut self) -> Result<Self::Output> {
        mkext2::build_file(&self.options, &self.output).map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(to_result(output))
    }
}

pub struct BuildBufferTask {
    options: mkext2::Options,
}

impl Task for BuildBufferTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> Result<Self::Output> {
        mkext2::build_vec(&self.options)
            .map(|(v, _)| v)
            .map_err(err)
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output.into())
    }
}

/// Build an image file. Runs on the libuv thread pool.
#[napi(ts_return_type = "Promise<BuildResult>")]
pub fn build(output: String, options: BuildOptions) -> Result<AsyncTask<BuildFileTask>> {
    Ok(AsyncTask::new(BuildFileTask {
        options: to_options(options)?,
        output: PathBuf::from(output),
    }))
}

/// Build an image file, blocking the event loop.
#[napi]
pub fn build_sync(output: String, options: BuildOptions) -> Result<BuildResult> {
    let opts = to_options(options)?;
    mkext2::build_file(&opts, &output)
        .map(to_result)
        .map_err(err)
}

/// Build an image in memory.
#[napi(ts_return_type = "Promise<Buffer>")]
pub fn build_buffer(options: BuildOptions) -> Result<AsyncTask<BuildBufferTask>> {
    Ok(AsyncTask::new(BuildBufferTask {
        options: to_options(options)?,
    }))
}

/// Outcome of [`mkext2`].
#[napi(object)]
pub struct RunResult {
    /// What the command line tool would print on standard output
    /// (`--version`, `--verbose`).
    pub stdout: String,
    /// Diagnostics, as the command line tool prints them on standard error.
    pub stderr: String,
    /// The image summary, when one was built.
    pub result: Option<BuildResult>,
}

pub struct RunTask {
    args: Vec<String>,
}

impl Task for RunTask {
    type Output = RunResult;
    type JsValue = RunResult;

    fn compute(&mut self) -> Result<Self::Output> {
        let args: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        match parse_args(&args).map_err(err)? {
            Command::Help => Ok(RunResult {
                stdout: String::new(),
                stderr: mkext2::cli::help_text("mkext2"),
                result: None,
            }),
            Command::Version => Ok(RunResult {
                stdout: format!("mkext2 {}\n", mkext2::cli::VERSION),
                stderr: String::new(),
                result: None,
            }),
            Command::Build {
                mut options,
                output,
                print_version,
            } => {
                if output == "-" {
                    return Err(err(
                        "writing the image to standard output is not supported here",
                    ));
                }
                let stderr = Arc::new(Mutex::new(String::new()));
                let sink = stderr.clone();
                options.message_sink = Some(Arc::new(move |m: &mkext2::Message| {
                    let mut s = sink.lock().unwrap();
                    if m.level == mkext2::Level::Warning {
                        s.push_str("mkext2: ");
                    }
                    s.push_str(&m.text);
                    s.push('\n');
                }));
                let report = mkext2::build_file(&options, PathBuf::from(&output)).map_err(|e| {
                    let s = stderr.lock().unwrap();
                    err(format!("{s}{e}"))
                })?;
                let mut stdout = String::new();
                if print_version {
                    stdout.push_str(&format!("mkext2 {}\n", mkext2::cli::VERSION));
                }
                if let Some(v) = &report.verbose_output {
                    stdout.push_str(v);
                }
                let stderr = stderr.lock().unwrap().clone();
                Ok(RunResult {
                    stdout,
                    stderr,
                    result: Some(to_result(report)),
                })
            }
        }
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

/// Run with command line arguments (without the program name), as the
/// `mkext2` tool takes them (the same as xgenext2fs'),
/// e.g. `["--block-size", "4096", "--faketime", "--root", "dir", "out.ext2"]`.
/// Rejects with the error message when the tool would exit with an error.
#[napi(js_name = "mkext2", ts_return_type = "Promise<RunResult>")]
pub fn mkext2_cli(args: Vec<String>) -> AsyncTask<RunTask> {
    AsyncTask::new(RunTask { args })
}

/// Version of the mkext2 library.
#[napi]
pub fn version() -> String {
    mkext2::cli::VERSION.to_string()
}
