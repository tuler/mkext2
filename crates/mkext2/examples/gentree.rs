//! Generate the random test trees used by the integration tests:
//! `cargo run --example gentree -- <dir> <seed> [depth] [entries] [max_file]`
#[path = "../tests/common/mod.rs"]
mod common;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: gentree <dir> <seed> [depth] [entries] [max_file]");
        std::process::exit(2);
    }
    let mut p = common::Profile::default();
    if let Some(v) = a.get(3) {
        p.max_depth = v.parse().unwrap();
    }
    if let Some(v) = a.get(4) {
        p.max_entries = v.parse().unwrap();
    }
    if let Some(v) = a.get(5) {
        p.max_file = v.parse().unwrap();
    }
    common::gen_tree(std::path::Path::new(&a[1]), a[2].parse().unwrap(), p);
}
