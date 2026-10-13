//! Reproducibility: the image depends only on the content and the options.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use common::*;
use mkext2::ondisk::S_IFMT;
use mkext2::reader::Image;
use mkext2::{Layer, Options, Readjustment, Timestamps, build_file, build_vec};

/// Create the same logical tree, inserting entries in a given order and
/// with given modification times.
fn make_tree(root: &Path, reverse: bool, mtime: u64) {
    let mut items: Vec<(String, Option<Vec<u8>>)> = vec![
        ("a".into(), None),
        ("a/b".into(), None),
        ("a/b/c.txt".into(), Some(b"c".to_vec())),
        ("Z-upper".into(), Some(vec![7u8; 5000])),
        ("z-lower".into(), Some(vec![9u8; 70_000])),
        (".hidden".into(), Some(b"hidden".to_vec())),
        ("dir2".into(), None),
        (
            "dir2/big".into(),
            Some((0..300_000u32).map(|i| (i * 31 % 251) as u8).collect()),
        ),
    ];
    for i in 0..60 {
        items.push((format!("dir2/f{i:03}"), Some(format!("{i}").into_bytes())));
    }
    if reverse {
        // parents must exist first: create directories up front
        for (p, c) in &items {
            if c.is_none() {
                fs::create_dir_all(root.join(p)).unwrap();
            }
        }
        items.reverse();
    }
    fs::create_dir_all(root).unwrap();
    for (p, c) in &items {
        match c {
            None => fs::create_dir_all(root.join(p)).unwrap(),
            Some(data) => fs::write(root.join(p), data).unwrap(),
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a/b/c.txt", root.join("link")).unwrap();
        fs::hard_link(root.join("z-lower"), root.join("dir2/hard")).unwrap();
    }
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(mtime);
    for e in walk(root) {
        let f = fs::File::options().read(true).open(&e);
        if let Ok(f) = f {
            let _ = f.set_modified(t);
        }
    }
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![root.to_path_buf()];
    let mut i = 0;
    while i < out.len() {
        let p = out[i].clone();
        if p.symlink_metadata().unwrap().is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                out.push(e.unwrap().path());
            }
        }
        i += 1;
    }
    out
}

fn opts(root: &Path, timestamps: Timestamps) -> Options {
    Options {
        block_size: 4096,
        timestamps,
        squash_uids: Some(0),
        readjustment: Readjustment::parse("+500").unwrap(),
        layers: vec![Layer::dir(root)],
        ..Options::default()
    }
}

#[test]
fn same_content_same_image() {
    let t = TempDir::new();
    let one = t.path().join("one");
    let two = t.path().join("deeper/path/two");
    make_tree(&one, false, 1_000_000);
    make_tree(&two, true, 2_000_000);
    for ts in [Timestamps::Zero, Timestamps::Fixed(1_700_000_000)] {
        let (a, _) = build_vec(&opts(&one, ts)).unwrap();
        let (b, _) = build_vec(&opts(&two, ts)).unwrap();
        assert!(a == b, "images differ for {ts:?}");
    }
    // with source timestamps, the different mtimes show
    let (a, _) = build_vec(&Options {
        source_date_epoch: Some(5),
        ..opts(&one, Timestamps::Source)
    })
    .unwrap();
    let (b, _) = build_vec(&Options {
        source_date_epoch: Some(5),
        ..opts(&two, Timestamps::Source)
    })
    .unwrap();
    assert!(a != b);
}

#[test]
fn repeated_builds_are_identical() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 123, Profile::default());
    let o = opts(&root, Timestamps::Zero);
    let (a, _) = build_vec(&o).unwrap();
    let (b, _) = build_vec(&o).unwrap();
    assert!(a == b);
    let f = t.path().join("out.img");
    build_file(&o, &f).unwrap();
    assert!(fs::read(&f).unwrap() == a, "file and memory outputs differ");
    // building over an existing, larger file gives the same bytes
    fs::write(&f, vec![0xFFu8; a.len() * 2]).unwrap();
    build_file(&o, &f).unwrap();
    assert!(fs::read(&f).unwrap() == a, "stale output content leaked");
    // CLI to a pipe
    let out = Command::new(ours())
        .args(["-B", "4096", "-f", "-U", "0", "-r", "+500", "-d"])
        .arg(&root)
        .arg("-")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout == a, "stdout output differs");
}

#[test]
fn fixed_timestamp_everywhere() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 5, Profile::default());
    let ts = 1_650_000_000u32;
    let (data, _) = build_vec(&opts(&root, Timestamps::Fixed(ts))).unwrap();
    let img = Image::new(data).unwrap();
    assert_eq!(img.superblock().wtime(), ts);
    assert_eq!(img.superblock().lastcheck(), ts);
    for n in img.walk().unwrap() {
        assert_eq!(
            (n.atime, n.ctime, n.mtime),
            (ts, ts, ts),
            "{}",
            String::from_utf8_lossy(&n.path)
        );
    }
}

#[test]
fn clamped_timestamps() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("old"), b"old").unwrap();
    fs::write(root.join("new"), b"new").unwrap();
    let set = |p: &str, secs: u64| {
        fs::File::options()
            .write(true)
            .open(root.join(p))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    };
    set("old", 1000);
    set("new", 3_000_000_000);
    let clamp = 2_000_000_000u32;
    let (data, _) = build_vec(&opts(&root, Timestamps::Clamp(clamp))).unwrap();
    let img = Image::new(data).unwrap();
    assert_eq!(img.superblock().wtime(), clamp);
    let old = img.inode(img.lookup(b"/old").unwrap().unwrap()).unwrap();
    let new = img.inode(img.lookup(b"/new").unwrap().unwrap()).unwrap();
    assert_eq!(old.mtime(), 1000);
    assert_eq!(new.mtime(), clamp);
    assert!(new.ctime() <= clamp);
}

#[test]
fn source_date_epoch_only_sets_filesystem_time() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    make_tree(&root, false, 1_234_567);
    let (data, _) = build_vec(&Options {
        source_date_epoch: Some(42),
        ..opts(&root, Timestamps::Source)
    })
    .unwrap();
    let img = Image::new(data).unwrap();
    assert_eq!(img.superblock().wtime(), 42);
    let root_ino = img.inode(2).unwrap();
    assert_eq!(root_ino.mtime(), 42);
    let f = img
        .inode(img.lookup(b"/a/b/c.txt").unwrap().unwrap())
        .unwrap();
    assert_eq!(f.mtime(), 1_234_567);
}

#[test]
fn tar_and_memory_sources_agree() {
    let Some(tar) = ["gtar", "tar"].into_iter().find(|t| {
        Command::new(t)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        return;
    };
    let t = TempDir::new();
    let root = t.path().join("tree");
    make_tree(&root, false, 99);
    let tarfile = t.path().join("t.tar");
    let st = Command::new(tar)
        .arg("-cf")
        .arg(&tarfile)
        .arg("-C")
        .arg(&root)
        .arg(".")
        .env("COPYFILE_DISABLE", "1")
        .status()
        .unwrap();
    assert!(st.success());
    let base = Options {
        block_size: 1024,
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+500").unwrap(),
        ..Options::default()
    };
    let from_file = Options {
        layers: vec![Layer::tar(&tarfile)],
        ..base.clone()
    };
    let from_bytes = Options {
        layers: vec![Layer::tar_bytes(fs::read(&tarfile).unwrap())],
        ..base
    };
    let (a, _) = build_vec(&from_file).unwrap();
    let (b, _) = build_vec(&from_bytes).unwrap();
    assert!(a == b);
    let img = Image::new(a).unwrap();
    assert!(img.check().unwrap().is_empty());
    let link = img.inode(img.lookup(b"/link").unwrap().unwrap()).unwrap();
    assert_eq!(u32::from(link.mode()) & S_IFMT, mkext2::ondisk::S_IFLNK);
}

#[test]
fn read_ahead_does_not_change_output() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        8080,
        Profile {
            max_depth: 4,
            max_entries: 30,
            max_file: 20_000_000,
            ..Profile::default()
        },
    );
    let mut images = Vec::new();
    for threads in [Some(1), Some(2), Some(16), None] {
        let o = Options {
            threads,
            ..opts(&root, Timestamps::Zero)
        };
        images.push(build_vec(&o).unwrap().0);
    }
    assert!(images.windows(2).all(|w| w[0] == w[1]));
}
