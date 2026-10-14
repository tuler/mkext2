//! Reproducible ext2 filesystem images from directories, tar archives and
//! device tables, without root privileges.
//!
//! [`build_file`], [`build_vec`] and [`build`] take [`Options`] (the
//! equivalent of the `mkext2` command line, see [`cli`]) and return a
//! [`Report`]. [`reader`] reads images back and checks them.

mod build;
pub mod cli;
pub mod device;
mod error;
mod fs;
mod hash;
mod layers;
pub mod ondisk;
mod options;
mod prefetch;
pub mod reader;
mod size;
mod source;
mod tree;

pub use build::{Output, build, build_file, build_on_device, build_vec};
pub use error::{Error, Result};
pub use options::{
    ImageInfo, Layer, LayerKind, Level, Message, MessageSink, Options, Readjusted, Readjustment,
    Report, Source, Timestamps, lookup_creator_os, si_atof,
};
