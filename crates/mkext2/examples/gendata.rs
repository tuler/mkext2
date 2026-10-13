//! Generate a benchmark dataset of roughly the given size, shaped like a
//! root filesystem: many small files, some medium ones, a few large ones.
//!
//! `cargo run --release --example gendata -- <dir> <total MiB> [seed]`
#[path = "../tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: gendata <dir> <total MiB> [seed]");
        std::process::exit(2);
    }
    let root = PathBuf::from(&a[1]);
    let total: u64 = a[2].parse::<u64>().unwrap() << 20;
    let seed: u64 = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(1);
    let mut rng = common::Rng::new(seed);
    fs::create_dir_all(&root).unwrap();

    // 50% of the bytes in a few large files, 30% medium, 20% small
    let mut dirs: Vec<PathBuf> = vec![root.clone()];
    let mut written = 0u64;
    let mut files = 0u64;
    let plan = [
        (total / 2, 8u64 << 20, 128u64 << 20),
        (total * 3 / 10, 64 << 10, 2 << 20),
        (total / 5, 0, 32 << 10),
    ];
    for (budget, lo, hi) in plan {
        let mut used = 0u64;
        while used < budget {
            // new directory now and then
            if rng.chance(4) || dirs.len() < 4 {
                let parent = rng.pick(&dirs).clone();
                if parent.components().count() - root.components().count() < 6 {
                    let d = parent.join(format!("d{}", dirs.len()));
                    fs::create_dir_all(&d).unwrap();
                    dirs.push(d);
                }
            }
            let size = rng.range(lo, hi).min(budget - used);
            let dir = rng.pick(&dirs).clone();
            write_file(&dir.join(format!("f{files}.dat")), size, &mut rng);
            used += size;
            written += size;
            files += 1;
        }
    }
    eprintln!(
        "{files} files, {} directories, {} MiB",
        dirs.len(),
        written >> 20
    );
}

fn write_file(p: &Path, size: u64, rng: &mut common::Rng) {
    let mut f = fs::File::create(p).unwrap();
    let text = rng.chance(50);
    let mut left = size;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        if text {
            for (i, b) in buf[..n].iter_mut().enumerate() {
                *b = b"lorem ipsum dolor sit amet, consectetur adipiscing elit\n"
                    [(i + left as usize) % 56];
            }
        } else {
            let r = rng.bytes(n);
            buf[..n].copy_from_slice(&r);
        }
        f.write_all(&buf[..n]).unwrap();
        left -= n as u64;
    }
}
