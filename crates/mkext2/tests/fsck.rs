//! Validate generated images with e2fsck (when available).

mod common;

use common::*;
use mkext2::{Layer, Options, Readjustment, Timestamps, build_file};

#[cfg(unix)]
#[test]
fn e2fsck_accepts_generated_images() {
    let t = TempDir::new();
    let img = t.path().join("check.img");
    if e2fsck(&img).is_none() {
        eprintln!("e2fsck not found: skipping");
        return;
    }
    let mut checked = 0;
    for seed in 1..=6u64 {
        let root = t.path().join(format!("tree{seed}"));
        gen_tree(
            &root,
            seed * 1000 + 7,
            Profile {
                max_depth: 3,
                max_entries: 15,
                max_file: 1_500_000,
                specials: true,
                max_symlink: 1023,
            },
        );
        for (bs, holes, blocks, inodes) in [
            (1024u32, false, None, None),
            (2048, true, None, None),
            (4096, false, None, None),
            (1024, false, Some(120_000i64), Some(20_000)),
            (4096, true, Some(40_000), None),
        ] {
            let opts = Options {
                block_size: bs,
                allow_holes: holes,
                size_in_blocks: blocks,
                number_of_inodes: inodes,
                timestamps: Timestamps::Fixed(1_700_000_000),
                readjustment: Readjustment::parse("+1000").unwrap(),
                layers: vec![Layer::dir(&root)],
                ..Options::default()
            };
            build_file(&opts, &img).unwrap();
            if let Some(Err(e)) = e2fsck(&img) {
                panic!("e2fsck failed for seed {seed}, block size {bs}:\n{e}");
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 30);
}
