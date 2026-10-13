//! Check two ext2 images and compare what they contain.
//!
//! `cargo run --release --example compare -- a.img b.img [--ignore-times]`
//!
//! Prints the consistency problems of each image and the differences in
//! content (paths, types, permissions, owners, times, data, link targets,
//! device numbers, hard links), and exits with status 1 if there are any.
use mkext2::reader::{CompareOptions, Image, compare_content};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if files.len() != 2 {
        eprintln!("usage: compare <a.img> <b.img> [--ignore-times]");
        std::process::exit(2);
    }
    let opts = CompareOptions {
        times: !args.iter().any(|a| a == "--ignore-times"),
        device_low_bits: true,
        ..Default::default()
    };
    let open = |p: &str| Image::open(p).unwrap_or_else(|e| panic!("{p}: {e}"));
    let (a, b) = (open(files[0]), open(files[1]));
    let mut bad = false;
    for (name, img) in [(files[0], &a), (files[1], &b)] {
        for p in img.check().unwrap() {
            println!("{name}: {p}");
            bad = true;
        }
    }
    for d in compare_content(&a, &b, &opts).unwrap() {
        println!("{d}");
        bad = true;
    }
    if bad {
        std::process::exit(1);
    }
    println!("valid and equivalent");
}
