//! Build images and read them back with the independent reader: the
//! content must match the sources and the metadata must be consistent.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use common::*;
use mkext2::ondisk::*;
use mkext2::reader::Image;
use mkext2::{Layer, Options, Readjustment, Timestamps, build_vec};

struct Expect {
    squash_uid: Option<u32>,
    squash_perms: bool,
    times: Option<u32>,
}

#[cfg(unix)]
fn verify_tree(img: &Image, src: &Path, dst: &[u8], exp: &Expect, inodes: &mut HashMap<u64, u32>) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let dir_nod = img.lookup(dst).unwrap().expect("directory missing");
    let mut entries: Vec<_> = fs::read_dir(src).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    let image_entries = img.read_dir(dir_nod).unwrap();
    let mut expected_names = vec![b".".to_vec(), b"..".to_vec()];
    for e in &entries {
        let name = e.file_name().as_bytes().to_vec();
        expected_names.push(name.clone());
        let md = fs::symlink_metadata(e.path()).unwrap();
        let mut path = dst.to_vec();
        if path != b"/" {
            path.push(b'/');
        }
        path.extend_from_slice(&name);
        let nod = img
            .lookup(&path)
            .unwrap()
            .unwrap_or_else(|| panic!("{} missing from image", String::from_utf8_lossy(&path)));
        let ino = img.inode(nod).unwrap();
        let ft = md.file_type();
        let want_type = if ft.is_dir() {
            S_IFDIR
        } else if ft.is_symlink() {
            S_IFLNK
        } else if ft.is_file() {
            S_IFREG
        } else if ft.is_fifo() {
            S_IFIFO
        } else if ft.is_socket() {
            S_IFSOCK
        } else if ft.is_char_device() {
            S_IFCHR
        } else {
            S_IFBLK
        };
        let shown = String::from_utf8_lossy(&path).into_owned();
        assert_eq!(ino.file_type(), want_type, "{shown}: type");
        if want_type != S_IFLNK {
            let mut perm = md.mode() & 0o7777;
            if exp.squash_perms {
                perm &= !0o077;
            }
            assert_eq!(u32::from(ino.mode()) & 0o7777, perm, "{shown}: permissions");
        } else {
            assert_eq!(
                u32::from(ino.mode()) & 0o7777,
                0o777,
                "{shown}: symlink permissions"
            );
        }
        let (uid, gid) = match exp.squash_uid {
            Some(id) => (id, id),
            None => (md.uid(), md.gid()),
        };
        assert_eq!(u32::from(ino.uid()), uid & 0xffff, "{shown}: uid");
        assert_eq!(u32::from(ino.gid()), gid & 0xffff, "{shown}: gid");
        match exp.times {
            Some(t) => {
                assert_eq!(ino.mtime(), t, "{shown}: mtime");
                assert_eq!(ino.ctime(), t, "{shown}: ctime");
                assert_eq!(ino.atime(), t, "{shown}: atime");
            }
            None => assert_eq!(ino.mtime(), md.mtime() as u32, "{shown}: mtime"),
        }
        if want_type != S_IFDIR && want_type != S_IFLNK && md.nlink() > 1 {
            if let Some(&first) = inodes.get(&md.ino()) {
                assert_eq!(nod, first, "{shown}: hard link should share the inode");
            } else {
                inodes.insert(md.ino(), nod);
            }
        }
        match want_type {
            S_IFREG => {
                let data = img.read_file(nod).unwrap();
                assert_eq!(data.len() as u64, md.len(), "{shown}: size");
                assert!(data == fs::read(e.path()).unwrap(), "{shown}: content");
            }
            S_IFLNK => {
                let target = fs::read_link(e.path()).unwrap();
                assert_eq!(
                    img.read_link(nod).unwrap(),
                    target.as_os_str().as_bytes(),
                    "{shown}: target"
                );
            }
            S_IFDIR => verify_tree(img, &e.path(), &path, exp, inodes),
            S_IFCHR | S_IFBLK => {
                let rdev = md.rdev();
                #[cfg(target_os = "linux")]
                let (maj, min) = (libc::major(rdev), libc::minor(rdev));
                #[cfg(not(target_os = "linux"))]
                let (maj, min) = (((rdev >> 24) & 0xff) as u32, (rdev & 0xffffff) as u32);
                assert_eq!(
                    mkext2::reader::decode_device(&ino),
                    (maj, min),
                    "{shown}: device numbers"
                );
            }
            _ => {}
        }
    }
    let mut got: Vec<Vec<u8>> = image_entries.iter().map(|e| e.name.clone()).collect();
    if dst == b"/" {
        got.retain(|n| n != b"lost+found");
    }
    got.sort();
    expected_names.sort();
    assert_eq!(
        got,
        expected_names,
        "entries of {}",
        String::from_utf8_lossy(dst)
    );
}

fn check_clean(img: &Image) {
    let problems = img.check().unwrap();
    assert!(problems.is_empty(), "image inconsistencies: {problems:#?}");
}

fn build(opts: &Options) -> Image {
    let (data, _) = build_vec(opts).unwrap();
    Image::new(data).unwrap()
}

#[cfg(unix)]
#[test]
fn directory_trees_roundtrip() {
    let t = TempDir::new();
    for seed in 1..=8u64 {
        let root = t.path().join(format!("tree{seed}"));
        gen_tree(
            &root,
            seed * 31,
            Profile {
                max_symlink: 1023,
                ..Profile::default()
            },
        );
        for bs in [1024, 2048, 4096] {
            for (holes, timestamps, squash_uid, squash_perms) in [
                (false, Timestamps::Zero, None, false),
                (true, Timestamps::Fixed(1_234_567), Some(0), true),
                (false, Timestamps::Source, Some(70_000), false),
            ] {
                let opts = Options {
                    block_size: bs,
                    allow_holes: holes,
                    timestamps,
                    source_date_epoch: Some(1),
                    squash_uids: squash_uid,
                    squash_perms,
                    readjustment: Readjustment::parse("+800").unwrap(),
                    layers: vec![Layer::dir(&root)],
                    ..Options::default()
                };
                let img = build(&opts);
                check_clean(&img);
                let exp = Expect {
                    squash_uid,
                    squash_perms,
                    times: match timestamps {
                        Timestamps::Zero => Some(0),
                        Timestamps::Fixed(t) => Some(t),
                        _ => None,
                    },
                };
                verify_tree(&img, &root, b"/", &exp, &mut HashMap::new());
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn multi_group_roundtrip() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        999,
        Profile {
            max_depth: 4,
            max_entries: 25,
            max_file: 3_000_000,
            specials: true,
            max_symlink: 1023,
        },
    );
    for (bs, blocks, inodes) in [
        (1024u32, 150_000i64, Some(20_000)),
        (2048, 60_000, None),
        (4096, 70_000, Some(70_000)),
    ] {
        let opts = Options {
            block_size: bs,
            size_in_blocks: Some(blocks),
            number_of_inodes: inodes,
            timestamps: Timestamps::Zero,
            layers: vec![Layer::dir(&root)],
            ..Options::default()
        };
        let img = build(&opts);
        assert!(img.group_descriptors().len() > 1);
        check_clean(&img);
        let exp = Expect {
            squash_uid: None,
            squash_perms: false,
            times: Some(0),
        };
        verify_tree(&img, &root, b"/", &exp, &mut HashMap::new());
    }
}

#[cfg(unix)]
#[test]
fn huge_file_with_holes() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    let mut rng = Rng::new(1);
    let mut data = rng.bytes(70 * 1024 * 1024 + 3);
    data[10_000..20_000_000].fill(0);
    data[60_000_000..69_000_000].fill(0);
    fs::write(root.join("big"), &data).unwrap();
    for holes in [false, true] {
        let opts = Options {
            block_size: 1024,
            allow_holes: holes,
            timestamps: Timestamps::Zero,
            layers: vec![Layer::dir(&root)],
            ..Options::default()
        };
        let img = build(&opts);
        let nod = img.lookup(b"/big").unwrap().unwrap();
        let ino = img.inode(nod).unwrap();
        assert!(ino.block(14) != 0, "should use triple indirection");
        assert!(img.read_file(nod).unwrap() == data);
        if holes {
            // blocks were skipped for the zero regions
            assert!(u64::from(ino.blocks()) * 512 < 60 * 1024 * 1024);
        } else {
            check_clean(&img);
        }
    }
}

#[cfg(unix)]
#[test]
fn tarball_roundtrip() {
    use std::process::{Command, Stdio};
    let Some(tar) = ["gtar", "tar"].into_iter().find(|t| {
        Command::new(t)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("GNU tar"))
    }) else {
        eprintln!("GNU tar not found: skipping");
        return;
    };
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        4242,
        Profile {
            specials: false,
            max_symlink: 1023,
            ..Profile::default()
        },
    );
    for fmt in ["gnu", "posix"] {
        let out = t.path().join(format!("{fmt}.tar.gz"));
        let st = Command::new(tar)
            .args([&format!("--format={fmt}"), "-czf"])
            .arg(&out)
            .arg("-C")
            .arg(&root)
            .arg(".")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        for bs in [1024, 4096] {
            let opts = Options {
                block_size: bs,
                timestamps: Timestamps::Fixed(42),
                readjustment: Readjustment::parse("+1000").unwrap(),
                layers: vec![Layer::tar(&out)],
                ..Options::default()
            };
            let img = build(&opts);
            check_clean(&img);
            let exp = Expect {
                squash_uid: None,
                squash_perms: false,
                times: Some(42),
            };
            verify_tree(&img, &root, b"/", &exp, &mut HashMap::new());
        }
    }
}

#[test]
fn layer_targets_are_honored() {
    let t = TempDir::new();
    let a = t.path().join("a");
    let b = t.path().join("b");
    fs::create_dir_all(a.join("opt/app")).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("hello"), b"hello").unwrap();
    let opts = Options {
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+300").unwrap(),
        layers: vec![Layer::dir(&a), Layer::dir(&b).at("/opt/app")],
        ..Options::default()
    };
    let img = build(&opts);
    check_clean(&img);
    let nod = img
        .lookup(b"/opt/app/hello")
        .unwrap()
        .expect("file under target");
    assert_eq!(img.read_file(nod).unwrap(), b"hello");
    assert!(img.lookup(b"/hello").unwrap().is_none());

    // a missing target is an error
    let bad = Options {
        layers: vec![Layer::dir(&b).at("/nope")],
        ..opts
    };
    let err = build_vec(&bad).unwrap_err().to_string();
    assert!(err.contains("not found"), "{err}");
}

#[test]
fn device_table_bytes() {
    let table = b"/dev d 755 0 0 - - - - -\n/dev/console c 600 0 0 5 1 - - -\n/dev/tty c 666 0 0 4 0 0 1 4\n/etc d 700 10 20\n/etc/pipe p 644 1 1\n";
    let opts = Options {
        timestamps: Timestamps::Zero,
        number_of_inodes: Some(64),
        size_in_blocks: Some(400),
        layers: vec![Layer::devtable_bytes(table.to_vec())],
        ..Options::default()
    };
    let img = build(&opts);
    check_clean(&img);
    let console = img
        .inode(img.lookup(b"/dev/console").unwrap().unwrap())
        .unwrap();
    assert_eq!(console.file_type(), S_IFCHR);
    assert_eq!(mkext2::reader::decode_device(&console), (5, 1));
    for i in 0..4u8 {
        let p = format!("/dev/tty{i}");
        let ino = img
            .inode(img.lookup(p.as_bytes()).unwrap().unwrap())
            .unwrap();
        assert_eq!(mkext2::reader::decode_device(&ino), (4, u32::from(i)));
        assert_eq!(ino.mode() & 0o7777, 0o666);
    }
    let etc = img.inode(img.lookup(b"/etc").unwrap().unwrap()).unwrap();
    assert_eq!((etc.mode() & 0o7777, etc.uid(), etc.gid()), (0o700, 10, 20));
    let pipe = img
        .inode(img.lookup(b"/etc/pipe").unwrap().unwrap())
        .unwrap();
    assert_eq!(pipe.file_type(), S_IFIFO);
}

#[test]
fn starting_image_extends_content() {
    let t = TempDir::new();
    let a = t.path().join("a");
    let b = t.path().join("b");
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::write(a.join("one"), vec![1u8; 5000]).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("two"), vec![2u8; 70_000]).unwrap();
    let base_path = t.path().join("base.img");
    let opts = Options {
        block_size: 4096,
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+2000").unwrap(),
        layers: vec![Layer::dir(&a)],
        ..Options::default()
    };
    mkext2::build_file(&opts, &base_path).unwrap();
    let base = fs::read(&base_path).unwrap();

    // add more files, in memory and in place
    let more = Options {
        starting_image: Some(mkext2::Source::Path(base_path.clone())),
        timestamps: Timestamps::Zero,
        layers: vec![Layer::dir(&b).at("/sub")],
        ..Options::default()
    };
    let (data, _) = build_vec(&more).unwrap();
    assert_eq!(data.len(), base.len());
    let img = Image::new(data.clone()).unwrap();
    check_clean(&img);
    let two = img.lookup(b"/sub/two").unwrap().unwrap();
    assert_eq!(img.read_file(two).unwrap(), vec![2u8; 70_000]);
    let one = img.lookup(b"/one").unwrap().unwrap();
    assert_eq!(img.read_file(one).unwrap(), vec![1u8; 5000]);

    mkext2::build_file(&more, &base_path).unwrap();
    assert!(
        fs::read(&base_path).unwrap() == data,
        "in-place update should match"
    );
}

#[cfg(unix)]
#[test]
fn long_symlinks_are_skipped() {
    // ext2 symlinks must fit in one block: longer ones are left out
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink("x".repeat(1000), root.join("ok")).unwrap();
    if std::os::unix::fs::symlink("y".repeat(1500), root.join("long")).is_err() {
        return; // the host does not allow such links
    }
    let opts = Options {
        block_size: 1024,
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+300").unwrap(),
        layers: vec![Layer::dir(&root)],
        ..Options::default()
    };
    let (data, report) = build_vec(&opts).unwrap();
    assert!(
        report.messages.iter().any(|m| m.text.contains("symlink")),
        "{:?}",
        report.messages
    );
    let img = Image::new(data).unwrap();
    check_clean(&img);
    assert!(img.lookup(b"/ok").unwrap().is_some());
    assert!(img.lookup(b"/long").unwrap().is_none());
}

#[test]
fn free_space_and_inodes() {
    // the readjustment is free space on top of the content and the reserve,
    // with inodes to use it
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        77,
        Profile {
            max_symlink: 1023,
            ..Profile::default()
        },
    );
    for (bs, extra) in [(1024u32, 5000u32), (4096, 2560), (4096, 0)] {
        let opts = Options {
            block_size: bs,
            timestamps: Timestamps::Zero,
            readjustment: Readjustment::parse(&format!("+{extra}")).unwrap(),
            layers: vec![Layer::dir(&root)],
            ..Options::default()
        };
        let (data, report) = build_vec(&opts).unwrap();
        let img = Image::new(data).unwrap();
        check_clean(&img);
        let sb = img.superblock();
        let usable = sb.free_blocks_count().saturating_sub(sb.r_blocks_count());
        assert!(
            usable >= extra,
            "bs {bs}: {usable} usable free blocks, asked for {extra}"
        );
        assert!(
            u64::from(sb.free_inodes_count()) * 16384 >= u64::from(usable) * u64::from(bs),
            "bs {bs}: not enough free inodes for the free space"
        );
        assert_eq!(report.info.blocks, sb.blocks_count());
        // the reserve is 5% of the image by default
        assert_eq!(sb.r_blocks_count(), sb.blocks_count() * 5 / 100);
    }
}

#[test]
fn archive_member_updates_existing_entry() {
    // a member naming something that exists updates that entry ("./"
    // updates the layer's target directory)
    fn header(name: &str, typeflag: u8, mode: u32, uid: u32) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
        h[108..116].copy_from_slice(format!("{uid:07o}\0").as_bytes());
        h[116..124].copy_from_slice(format!("{uid:07o}\0").as_bytes());
        h[124..136].copy_from_slice(b"00000000000\0");
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = typeflag;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }
    let mut tar = Vec::new();
    for h in [
        header("./", b'5', 0o750, 5),
        header("dir/", b'5', 0o755, 1),
        header("dir/", b'5', 0o700, 2),
    ] {
        tar.extend_from_slice(&h);
    }
    tar.extend_from_slice(&[0u8; 1024]);
    let opts = Options {
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+300").unwrap(),
        layers: vec![Layer::tar_bytes(tar)],
        ..Options::default()
    };
    let img = build(&opts);
    check_clean(&img);
    let root = img.inode(2).unwrap();
    assert_eq!((root.mode() & 0o7777, root.uid()), (0o750, 5));
    let dir = img.inode(img.lookup(b"/dir").unwrap().unwrap()).unwrap();
    assert_eq!((dir.mode() & 0o7777, dir.uid(), dir.gid()), (0o700, 2, 2));
}

#[test]
fn fill_value_fills_free_blocks() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        3,
        Profile {
            max_symlink: 1023,
            ..Profile::default()
        },
    );
    for bs in [1024u32, 4096] {
        let opts = Options {
            block_size: bs,
            fill_value: 0xA5,
            timestamps: Timestamps::Zero,
            readjustment: Readjustment::parse("+200").unwrap(),
            layers: vec![Layer::dir(&root)],
            ..Options::default()
        };
        let img = build(&opts);
        check_clean(&img);
        let mut free = 0;
        for b in img.superblock().first_data_block()..img.superblock().blocks_count() {
            if !img.block_used(b).unwrap() {
                free += 1;
                assert!(
                    img.block(b).unwrap().iter().all(|&c| c == 0xA5),
                    "free block {b} not filled"
                );
            }
        }
        assert_eq!(free, img.superblock().free_blocks_count());
    }
}

#[test]
fn long_names_are_skipped() {
    // ext2 names are at most 255 bytes: a longer archive member is left out
    fn header(name: &[u8], typeflag: u8, size: usize, gnu: bool) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name);
        h[100..108].copy_from_slice(b"0000644\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[156] = typeflag;
        h[257..265].copy_from_slice(if gnu { b"ustar  \0" } else { b"ustar\x0000" });
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }
    let long = vec![b'g'; 300];
    let mut tar = Vec::new();
    let mut body = long.clone();
    body.push(0);
    tar.extend_from_slice(&header(b"././@LongLink", b'L', body.len(), true));
    body.resize(512, 0);
    tar.extend_from_slice(&body);
    tar.extend_from_slice(&header(b"short", b'0', 3, true));
    let mut data = b"abc".to_vec();
    data.resize(512, 0);
    tar.extend_from_slice(&data);
    tar.extend_from_slice(&header(b"kept", b'0', 0, false));
    tar.extend_from_slice(&[0u8; 1024]);
    let opts = Options {
        timestamps: Timestamps::Zero,
        readjustment: Readjustment::parse("+100").unwrap(),
        layers: vec![Layer::tar_bytes(tar)],
        ..Options::default()
    };
    let (data, report) = build_vec(&opts).unwrap();
    assert!(
        report
            .messages
            .iter()
            .any(|m| m.level == mkext2::Level::Warning
                && m.text.contains("ggggg")
                && m.text.contains("255")),
        "{:?}",
        report.messages
    );
    let img = Image::new(data).unwrap();
    check_clean(&img);
    assert!(img.lookup(b"/kept").unwrap().is_some());
    let names: Vec<Vec<u8>> = img
        .read_dir(2)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.iter().all(|n| n.len() <= 255));
}
