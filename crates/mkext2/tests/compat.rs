//! Differential tests: for the same inputs and options, mkext2's image must
//! be valid and hold the same content as xgenext2fs' (the reference
//! implementation, from `$XGENEXT2FS` or `xgenext2fs` on the PATH).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::*;

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

fn have_reference() -> bool {
    if reference().is_none() {
        eprintln!("xgenext2fs not found: skipping differential test");
        return false;
    }
    true
}

/// GNU tar if available (needed for --format and --sparse).
fn gnu_tar() -> Option<String> {
    for cand in ["gtar", "tar"] {
        if let Ok(o) = Command::new(cand).arg("--version").output() {
            if o.status.success() && String::from_utf8_lossy(&o.stdout).contains("GNU tar") {
                return Some(cand.to_string());
            }
        }
    }
    None
}

fn make_tar(tar: &str, src: &Path, out: &Path, extra: &[&str]) -> bool {
    let st = Command::new(tar)
        .stdin(Stdio::null())
        .args(extra)
        .arg("-cf")
        .arg(out)
        .arg("-C")
        .arg(src)
        .arg(".")
        .env("COPYFILE_DISABLE", "1")
        .stderr(Stdio::null())
        .status()
        .unwrap();
    st.success()
}

#[test]
fn random_trees() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    for seed in 1..=10u64 {
        let root = t.path().join(format!("tree{seed}"));
        gen_tree(&root, seed, Profile::default());
        for bs in ["1024", "2048", "4096"] {
            assert_equivalent(t.path(), &["-B", bs, "-f", "-r", "+600", "-d", &s(&root)]);
        }
        // without faketime: inode times come from the files
        assert_equivalent(t.path(), &["-B", "4096", "-r", "+600", "-d", &s(&root)]);
        // Cartesi CLI style
        assert_equivalent(
            t.path(),
            &[
                "--block-size",
                "4096",
                "--faketime",
                "--readjustment",
                "+1000",
                "--root",
                &s(&root),
            ],
        );
    }
}

#[test]
fn option_matrix() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 42, Profile::default());
    let r = s(&root);
    let cases: Vec<Vec<&str>> = vec![
        vec!["-f", "-r", "+500", "-d", &r],
        vec!["-f", "-r", "+30%", "-d", &r],
        vec!["-f", "-r", "+1k", "-d", &r],
        vec!["-f", "-r", "+2ki", "-d", &r],
        vec!["-f", "-r", "150%", "-d", &r],
        vec!["-f", "-b", "8000", "-d", &r],
        vec!["-f", "-b", "8k", "-d", &r],
        vec!["-f", "-b", "20000", "-N", "3000", "-d", &r],
        vec!["-f", "-b", "20000", "-i", "4096", "-d", &r],
        vec!["-f", "-b", "20000", "-i", "1Ki", "-d", &r],
        vec!["-f", "-r", "+500", "-N", "10", "-d", &r],
        vec!["-f", "-r", "+500", "-m", "0", "-d", &r],
        vec!["-f", "-r", "+500", "-m", "12.5", "-d", &r],
        vec!["-f", "-r", "+500", "-m", "50", "-d", &r],
        vec!["-f", "-r", "+500", "-L", "my-volume", "-d", &r],
        vec![
            "-f",
            "-r",
            "+500",
            "-L",
            "a-very-long-volume-label-indeed",
            "-d",
            &r,
        ],
        vec!["-f", "-r", "+500", "-e", "170", "-d", &r],
        vec!["-f", "-r", "+500", "-e", "255", "-B", "4096", "-d", &r],
        vec!["-f", "-r", "+500", "-e", "1", "-B", "2048", "-d", &r],
        vec!["-f", "-r", "+500", "-o", "hurd", "-d", &r],
        vec!["-f", "-r", "+500", "-o", "FreeBSD", "-d", &r],
        vec!["-f", "-r", "+500", "-o", "5", "-d", &r],
        vec!["-f", "-r", "+500", "-U", "1234", "-d", &r],
        vec!["-f", "-r", "+500", "-U", "70000", "-d", &r],
        vec!["-f", "-r", "+500", "-P", "-d", &r],
        vec!["-f", "-r", "+500", "--squash=7", "-d", &r],
        vec!["-f", "-r", "+500", "--squash", "0", "-d", &r],
        vec!["-f", "-r", "+500", "-U", "-1", "-d", &r],
        vec!["-r", "+500", "-d", &r, "-B", "2048"],
        vec!["-f", "-r", "+500", "-z", "-d", &r],
        vec!["-f", "-r", "+500", "-z", "-B", "4096", "-d", &r],
        vec!["-fzP", "-r+500", "-B4096", "-d", &r],
        vec!["--fake", "--block-s=2048", "--read=+500", "--ro", &r],
        vec!["-f", "-r", "+500", "-v", "-d", &r],
        vec!["-f", "-r", "+500", "-v", "-B", "4096", "-d", &r],
        vec!["-f", "-r", "+500", "-g", "/", "-d", &r],
        vec!["-f", "-r", "+500", "-g", "/lost+found", "-g", "/", "-d", &r],
    ];
    for c in &cases {
        assert_equivalent(t.path(), c);
    }
}

#[test]
fn block_maps_of_files() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(root.join("sub/dir")).unwrap();
    let mut rng = Rng::new(9);
    fs::write(root.join("sub/dir/file"), rng.bytes(300_000)).unwrap();
    fs::write(root.join("small"), b"x").unwrap();
    let r = s(&root);
    for bs in ["1024", "4096"] {
        assert_equivalent(
            t.path(),
            &[
                "-f",
                "-B",
                bs,
                "-r",
                "+300",
                "-g",
                "/sub/dir/file",
                "-g",
                "small",
                "-g",
                "/sub",
                "-d",
                &r,
            ],
        );
    }
}

#[test]
fn errors() {
    // mkext2 must reject these (whatever xgenext2fs does)
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 7, Profile::default());
    let r = s(&root);
    let missing_target = format!("{r}:/no/such/target");
    let cases: Vec<Vec<&str>> = vec![
        vec!["-f", "-B", "1000", "-d", &r],
        vec!["-f", "-r", "+12x", "-d", &r],
        vec!["-f", "-d", "/nonexistent/dir"],
        vec!["-f", "-a", "/nonexistent/tar"],
        vec!["-f", "-a", &r],
        vec!["-f", "-D", &r],
        vec!["-f", "-d", &r, "extra-arg"],
        vec!["--bogus-option"],
        vec!["-f", "--squ", "1", "-d", &r],
        vec!["-f", "-m", "-1", "-d", &r],
        vec!["-f", "-d", &missing_target],
    ];
    for c in &cases {
        let mut a: Vec<&std::ffi::OsStr> = c.iter().map(std::ffi::OsStr::new).collect();
        a.push(std::ffi::OsStr::new("image.img"));
        let o = run(&ours(), &a, t.path());
        assert_eq!(o.status.code(), Some(1), "{c:?} should fail");
        assert!(!o.stderr.is_empty(), "{c:?} should explain the failure");
    }
}

#[test]
fn tight_sizes() {
    // cases where xgenext2fs' estimate is too small or its limits are hit:
    // mkext2 has to produce a valid image with the same content
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 7, Profile::default());
    let r = s(&root);
    let empty = t.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    let e = s(&empty);
    for c in [
        vec!["-f", "-B", "4096", "-d", &r],
        vec!["-f", "-B", "1024", "-d", &e],
        vec!["-f", "-B", "2048", "-d", &r],
        vec!["-f", "-b", "4", "-d", &e],
        vec!["-f", "-N", "3", "-b", "100", "-d", &e],
        vec!["-f", "-m", "0", "-d", &r],
    ] {
        assert_equivalent(t.path(), &c);
    }
}

#[test]
fn empty_images() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    // cartesi CLI `empty` drives
    for blocks in ["8", "100", "256", "1000", "8192", "32768", "65536"] {
        let img = assert_equivalent(
            t.path(),
            &[
                "--block-size",
                "4096",
                "--faketime",
                "--size-in-blocks",
                blocks,
            ],
        );
        let n: u32 = blocks.parse().unwrap();
        if n >= 100 {
            let img = mkext2::reader::Image::new(img.expect("image")).unwrap();
            assert_eq!(
                img.superblock().blocks_count(),
                n,
                "--size-in-blocks {n} is exact"
            );
        }
    }
    for blocks in ["20", "100", "8193", "16385", "100000"] {
        assert_equivalent(t.path(), &["-B", "1024", "-f", "-b", blocks]);
        assert_equivalent(t.path(), &["-B", "2048", "-f", "-b", blocks]);
    }
}

#[test]
fn multiple_groups() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(
        &root,
        77,
        Profile {
            max_depth: 4,
            max_entries: 20,
            max_file: 2_000_000,
            specials: true,
            ..Profile::default()
        },
    );
    let r = s(&root);
    let cases: Vec<Vec<&str>> = vec![
        vec!["-f", "-B", "1024", "-N", "30000", "-b", "40000", "-d", &r],
        vec!["-f", "-B", "1024", "-b", "200000", "-d", &r],
        vec!["-f", "-B", "1024", "-b", "200000", "-N", "40000", "-d", &r],
        vec!["-f", "-B", "2048", "-b", "100000", "-d", &r],
        vec!["-f", "-B", "4096", "-b", "100000", "-N", "100000", "-d", &r],
        vec!["-f", "-B", "1024", "-r", "+50000", "-d", &r],
        vec!["-f", "-B", "1024", "-b", "200000", "-e", "7", "-d", &r],
        vec!["-f", "-B", "4096", "-b", "100000", "-e", "7", "-d", &r],
    ];
    for c in &cases {
        assert_equivalent(t.path(), c);
    }
}

#[test]
fn deep_indirection() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    let mut rng = Rng::new(5);
    // with 1 KiB blocks, triple indirection starts after 12 + 256 + 65536 blocks
    let mut big = rng.bytes(66 * 1024 * 1024 + 12345);
    big[1_000_000..3_000_000].fill(0);
    big[40_000_000..41_000_000].fill(0);
    fs::write(root.join("big"), &big).unwrap();
    fs::write(root.join("after"), b"after the big file").unwrap();
    let r = s(&root);
    assert_equivalent(t.path(), &["-f", "-B", "1024", "-r", "+100", "-d", &r]);
    assert_equivalent(
        t.path(),
        &["-f", "-B", "1024", "-z", "-r", "+100", "-d", &r],
    );
    assert_equivalent(t.path(), &["-f", "-B", "2048", "-r", "+100", "-d", &r]);
}

#[test]
fn wide_directories() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let root = t.path().join("tree");
    let mut rng = Rng::new(11);
    gen_wide_dir(&root.join("wide"), &mut rng, 3000);
    gen_wide_dir(&root.join("other"), &mut rng, 300);
    let r = s(&root);
    for bs in ["1024", "2048", "4096"] {
        assert_equivalent(t.path(), &["-f", "-B", bs, "-r", "+500", "-d", &r]);
    }
}

#[test]
fn layered_directories() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let a = t.path().join("a");
    let b = t.path().join("b");
    gen_tree(&a, 100, Profile::default());
    gen_tree(&b, 101, Profile::default());
    // overlapping names: same directories and files in both
    for d in [&a, &b] {
        fs::create_dir_all(d.join("common/nested")).unwrap();
        fs::write(d.join("common/file"), d.to_str().unwrap()).unwrap();
        fs::write(d.join("common/nested/x"), b"x").unwrap();
    }
    fs::write(b.join("common/nested/only-b"), b"b").unwrap();
    let (a, b) = (s(&a), s(&b));
    assert_equivalent(t.path(), &["-f", "-r", "+800", "-d", &a, "-d", &b]);
    assert_equivalent(
        t.path(),
        &["-f", "-r", "+800", "-d", &b, "-d", &a, "-B", "4096"],
    );
    assert_equivalent(t.path(), &["-f", "-r", "+800", "-d", &a, "-d", &a]);
}

#[test]
fn device_tables() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    // device tables in the mkfs.jffs2 format
    let table = t.path().join("devices.txt");
    fs::write(
        &table,
        "# a typical /dev\n\
         /dev d 755 0 0 - - - - -\n\
         /dev/console c 600 0 0 5 1 - - -\n\
         /dev/null c 666 0 0 1 3 0 0 -\n\
         /dev/zero c 666 0 0 1 5 0 0 -\n\
         /dev/tty c 666 0 0 5 0 0 0 -\n\
         /dev/tty c 620 0 5 4 0 0 1 8\n\
         /dev/ram b 640 0 6 1 0 0 1 4\n\
         /dev/mmcblk0p b 660 0 6 179 1 1 1 7\n\
         /dev/pts d 755 0 0 - - - - -\n\
         /dev/shm d 1777 0 0 - - - - -\n\
         /run d 755 0 0\n\
         /run/initctl p 600 0 0\n",
    )
    .unwrap();
    // change the owner of an existing symlink
    let link_table = t.path().join("chown-link.txt");
    fs::write(&link_table, "/symlink l 777 77 7 - - - - -\n").unwrap();
    let root = t.path().join("tree");
    fs::create_dir_all(root.join("dev")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        "12345678901234567890123456789012345678901234567890123456",
        root.join("symlink"),
    )
    .unwrap();
    // a table exercising odd lines
    let odd = t.path().join("odd.txt");
    fs::write(
        &odd,
        "# comment\n\n/ d 700 1 2\n/ c 700 1 2\n/dev/null c 666 0 0 1 3\n/dev/x/y c 666 0 0 1 3\n\
         /dev/. d 755 0 0\n/dev/bad q 755 0 0\n/onlyname\n/two d\n/etc d 755 0 0 - - - - -\n\
         /etc/fifo p 600 5 5\n/etc/sock s 600 5 5\n/etc/reg f 640 1 1\n/etc/lnk l 777 1 1\n\
         /dev/ttyS c 600 0 0 4 64 0 1 4\n/dev/sd b 600 0 0 8 0 1 16 4\n/dev/null c 600 9 9 1 3\n\
         /dev/wrap c 600 0 0 1 250 0 3 5\n",
    )
    .unwrap();
    let (tb, lt, od, r) = (s(&table), s(&link_table), s(&odd), s(&root));
    assert_equivalent(t.path(), &["-f", "-N", "92", "-b", "4096", "-D", &tb]);
    assert_equivalent(t.path(), &["-f", "-r", "+300", "-D", &tb]);
    assert_equivalent(t.path(), &["-f", "-r", "+300", "-d", &r, "-D", &lt]);
    assert_equivalent(
        t.path(),
        &["-f", "-r", "+300", "-B", "4096", "-d", &r, "-D", &lt],
    );
    assert_equivalent(t.path(), &["-f", "-r", "+300", "-D", &od]);
    assert_equivalent(t.path(), &["-r", "+300", "-D", &od]);
    assert_equivalent(t.path(), &["-f", "-r", "+300", "-D", &od, "-D", &tb, "-v"]);
}

#[test]
fn tarballs() {
    if !have_reference() {
        return;
    }
    let Some(tar) = gnu_tar() else {
        eprintln!("GNU tar not found: skipping");
        return;
    };
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 2024, Profile::default());
    let mut rng = Rng::new(3);
    gen_wide_dir(&root.join("wide"), &mut rng, 200);
    // long names and link targets beyond the 100 byte ustar fields
    let deep = root
        .join("x".repeat(90))
        .join("y".repeat(90))
        .join("z".repeat(90));
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("w".repeat(120)), b"deep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("t".repeat(150), root.join("longlink")).unwrap();

    let mut tars: Vec<PathBuf> = Vec::new();
    for fmt in ["gnu", "oldgnu", "posix", "ustar", "v7"] {
        let out = t.path().join(format!("{fmt}.tar"));
        // ustar/v7 cannot store everything; tar still writes what it can
        make_tar(&tar, &root, &out, &[&format!("--format={fmt}")]);
        if out.exists() {
            tars.push(out);
        }
    }
    let posix = t.path().join("posix.tar");
    for (flag, ext, cmd) in [
        ("-z", "gz", "gzip"),
        ("-j", "bz2", "bzip2"),
        ("-J", "xz", "xz"),
        ("--zstd", "zst", "zstd"),
    ] {
        let probe = Command::new(cmd)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if probe.is_err() {
            continue;
        }
        let out = t.path().join(format!("posix.tar.{ext}"));
        if make_tar(&tar, &root, &out, &["--format=posix", flag]) {
            tars.push(out);
        }
    }
    // double compression
    let gz = t.path().join("posix.tar.gz");
    if gz.exists() {
        let gzgz = t.path().join("posix.tar.gz.gz");
        let st = Command::new("gzip")
            .arg("-c")
            .arg(&gz)
            .stdout(fs::File::create(&gzgz).unwrap())
            .status();
        if st.is_ok_and(|s| s.success()) {
            tars.push(gzgz);
        }
    }
    assert!(posix.exists());
    for tf in &tars {
        let tf = s(tf);
        assert_equivalent(t.path(), &["-f", "-r", "+800", "-a", &tf]);
        assert_equivalent(t.path(), &["-r", "+800", "-B", "4096", "-a", &tf]);
        assert_equivalent(
            t.path(),
            &[
                "--block-size",
                "4096",
                "--faketime",
                "--readjustment",
                "+1000",
                "--tarball",
                &tf,
            ],
        );
    }
    // squashing applies to archives too
    let p = s(&posix);
    // (short -q makes xgenext2fs crash: it reads a missing argument)
    assert_equivalent(t.path(), &["-f", "-r", "+800", "--squash=0", "-a", &p]);
    assert_equivalent(t.path(), &["-f", "-r", "+800", "-U", "9", "-P", "-a", &p]);
    assert_equivalent(t.path(), &["-f", "-r", "+800", "-z", "-a", &p]);
    // (archives over existing entries: xgenext2fs changes the parent
    // directory instead of the entry; see archive_member_updates_existing_entry)
}

#[test]
fn sparse_tarballs() {
    if !have_reference() {
        return;
    }
    let Some(tar) = gnu_tar() else {
        return;
    };
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    // files with real holes, including trailing holes
    for (name, layout) in [
        ("lead", vec![(0u64, 0u64), (5_000_000, 7000)]),
        ("trail", vec![(0, 10_000), (3_000_000, 0)]),
        (
            "mixed",
            vec![(100_000, 4096), (900_000, 12345), (2_000_000, 1)],
        ),
        ("odd", vec![(0, 777), (70_000, 333), (99_999, 0)]),
    ] {
        let mut f = fs::File::create(root.join(name)).unwrap();
        let mut end = 0;
        for (off, len) in layout {
            use std::io::{Seek, SeekFrom};
            f.seek(SeekFrom::Start(off)).unwrap();
            f.write_all(&vec![0xAB; len as usize]).unwrap();
            end = end.max(off + len);
        }
        f.set_len(end.max(1)).unwrap();
    }
    for (fmt, extra) in [
        ("gnu", None),
        ("posix", Some("--sparse-version=0.1")),
        ("posix", Some("--sparse-version=1.0")),
        ("posix", Some("--sparse-version=0.0")),
    ] {
        let out = t.path().join(format!(
            "sparse-{fmt}-{}.tar",
            extra.unwrap_or("old").replace(['-', '='], "")
        ));
        let mut args = vec![format!("--format={fmt}"), "--sparse".to_string()];
        if let Some(e) = extra {
            args.push(e.to_string());
        }
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        if !make_tar(&tar, &root, &out, &a) {
            continue;
        }
        let o = s(&out);
        for bs in ["1024", "4096"] {
            assert_equivalent(t.path(), &["-f", "-B", bs, "-r", "+5000", "-a", &o]);
        }
    }
}

/// Hand-crafted archives with entries tar tools rarely produce.
#[test]
fn crafted_tarballs() {
    if !have_reference() {
        return;
    }
    let t = TempDir::new();
    let mut w = TarWriter::default();
    w.entry("./", b'5', 0o755, b"", b"", 0, 0, false);
    w.entry("dir", b'5', 0o750, b"", b"", 0, 0, false);
    w.entry("dir/file", b'0', 0o644, b"hello world", b"", 0, 0, false);
    w.entry("dir/chr", b'3', 0o600, b"", b"", 4, 64, false);
    w.entry("dir/blk", b'4', 0o600, b"", b"", 8, 300, false);
    w.entry("dir/fifo", b'6', 0o644, b"", b"", 0, 0, false);
    w.entry("dir/link", b'1', 0o644, b"", b"dir/file", 0, 0, false);
    w.entry("dir/abslink", b'1', 0o644, b"", b"/dir/file", 0, 0, false);
    w.entry("dir/sym", b'2', 0o777, b"", b"file", 0, 0, false);
    w.entry("/abs/ignored", b'0', 0o644, b"x", b"", 0, 0, false); // missing parent
    w.entry("/dir/absfile", b'0', 0o644, b"abs", b"", 0, 0, false);
    w.entry("olddir/", b'0', 0o755, b"", b"", 0, 0, true); // v7 style directory
    w.entry("olddir/f", b'0', 0o755, b"v7", b"", 0, 0, true);
    w.entry("contig", b'7', 0o644, b"contiguous", b"", 0, 0, false);
    w.entry("weird", b'Z', 0o644, b"unknown type", b"", 0, 0, false);
    w.entry(
        "modetype",
        b'0',
        0o100644,
        b"mode with type bits",
        b"",
        0,
        0,
        false,
    );
    w.entry("hlmode", b'1', 0o100644, b"", b"dir/file", 0, 0, false);
    w.pax(&[
        ("path", "pax/name/is/long/and/goes/here"),
        ("mtime", "1600000000.25"),
    ]);
    w.entry("pax", b'5', 0o755, b"", b"", 0, 0, false);
    w.pax(&[
        ("path", "pax-file"),
        ("uid", "100000"),
        ("gid", "200000"),
        ("size", "3"),
    ]);
    w.entry("ignored-name", b'0', 0o644, b"abc", b"", 0, 0, false);
    w.gnu_long(b'L', "g".repeat(300).as_bytes());
    w.entry("short", b'0', 0o644, b"long gnu name", b"", 0, 0, false);
    w.gnu_long(b'K', "k".repeat(200).as_bytes());
    w.entry("longsym", b'2', 0o777, b"", b"short", 0, 0, false);
    w.entry("big-uid", b'0', 0o644, b"", b"", 0, 0, false);
    let tarfile = t.path().join("crafted.tar");
    fs::write(&tarfile, w.finish()).unwrap();
    let tf = s(&tarfile);
    for bs in ["1024", "4096"] {
        assert_equivalent(t.path(), &["-f", "-B", bs, "-r", "+300", "-a", &tf]);
        assert_equivalent(t.path(), &["-B", bs, "-r", "+300", "-a", &tf]);
    }
}

#[test]
fn starting_image() {
    if !have_reference() {
        return;
    }
    let reference = reference().unwrap();
    let t = TempDir::new();
    let a = t.path().join("a");
    let b = t.path().join("b");
    gen_tree(&a, 300, Profile::default());
    gen_tree(&b, 301, Profile::default());
    fs::create_dir_all(a.join("target/dir")).unwrap();
    for bs in ["1024", "4096"] {
        let base = t.path().join(format!("base-{bs}.img"));
        let st = Command::new(&reference)
            .args(["-f", "-B", bs, "-r", "+3000", "-d"])
            .arg(&a)
            .arg(&base)
            .env("SOURCE_DATE_EPOCH", "1")
            .status()
            .unwrap();
        assert!(st.success());
        let (bp, bb) = (s(&base), s(&b));
        let tgt = format!("{bb}:/target/dir");
        assert_equivalent(t.path(), &["-B", bs, "-x", &bp, "-d", &bb]);
        assert_equivalent(t.path(), &["-B", bs, "-f", "-x", &bp, "-d", &tgt]);
        assert_equivalent(
            t.path(),
            &["-B", bs, "-f", "-x", &bp, "-d", &bb, "-L", "relabel"],
        );
    }
}

#[test]
fn stdin_and_stdout() {
    if !have_reference() {
        return;
    }
    let Some(tar) = gnu_tar() else {
        return;
    };
    let reference = reference().unwrap();
    let t = TempDir::new();
    let root = t.path().join("tree");
    gen_tree(&root, 55, Profile::default());
    let tarfile = t.path().join("in.tar");
    assert!(make_tar(&tar, &root, &tarfile, &[]));
    // xgenext2fs truncates its standard output, so it must be a file
    let outfile = t.path().join("stdout.img");
    let run_with = |bin: &Path, args: &[&str]| -> Vec<u8> {
        let out = Command::new(bin)
            .args(args)
            .stdin(fs::File::open(&tarfile).unwrap())
            .stdout(fs::File::create(&outfile).unwrap())
            .env("SOURCE_DATE_EPOCH", "1700000000")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        fs::read(&outfile).unwrap()
    };
    for args in [
        vec!["-f", "-N", "500", "-b", "20000", "-a", "-", "-"],
        vec![
            "-f",
            "-r",
            "+3000",
            "-N",
            "500",
            "-B",
            "4096",
            "-d",
            root.to_str().unwrap(),
            "-",
        ],
    ] {
        let a = run_with(&reference, &args);
        let b = run_with(&ours(), &args);
        assert!(!a.is_empty());
        let (ra, rb) = (
            mkext2::reader::Image::new(a).unwrap(),
            mkext2::reader::Image::new(b).unwrap(),
        );
        assert!(rb.check().unwrap().is_empty(), "invalid image for {args:?}");
        let opts = mkext2::reader::CompareOptions {
            device_low_bits: true,
            ..Default::default()
        };
        let diffs = mkext2::reader::compare_content(&ra, &rb, &opts).unwrap();
        assert!(diffs.is_empty(), "{args:?}: {diffs:#?}");
    }
}

/// Minimal tar writer for crafted archives.
#[derive(Default)]
struct TarWriter {
    data: Vec<u8>,
}

impl TarWriter {
    #[allow(clippy::too_many_arguments)]
    fn header(
        name: &[u8],
        typeflag: u8,
        mode: u32,
        size: usize,
        link: &[u8],
        major: u32,
        minor: u32,
        v7: bool,
    ) -> [u8; 512] {
        let mut h = [0u8; 512];
        let n = name.len().min(100);
        h[..n].copy_from_slice(&name[..n]);
        h[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
        h[108..116].copy_from_slice(b"0001750\0");
        h[116..124].copy_from_slice(b"0001750\0");
        h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        h[136..148].copy_from_slice(b"14000000000\0");
        h[156] = typeflag;
        let l = link.len().min(100);
        h[157..157 + l].copy_from_slice(&link[..l]);
        if !v7 {
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            h[265..270].copy_from_slice(b"user\0");
            h[297..303].copy_from_slice(b"group\0");
            h[329..337].copy_from_slice(format!("{major:07o}\0").as_bytes());
            h[337..345].copy_from_slice(format!("{minor:07o}\0").as_bytes());
        }
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h
    }

    fn push(&mut self, h: [u8; 512], body: &[u8]) {
        self.data.extend_from_slice(&h);
        self.data.extend_from_slice(body);
        let pad = (512 - body.len() % 512) % 512;
        self.data.extend(std::iter::repeat_n(0u8, pad));
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        &mut self,
        name: &str,
        typeflag: u8,
        mode: u32,
        body: &[u8],
        link: &[u8],
        major: u32,
        minor: u32,
        v7: bool,
    ) {
        let h = Self::header(
            name.as_bytes(),
            typeflag,
            mode,
            body.len(),
            link,
            major,
            minor,
            v7,
        );
        self.push(h, body);
    }

    fn pax(&mut self, recs: &[(&str, &str)]) {
        let mut body = Vec::new();
        for (k, v) in recs {
            let rest = format!(" {k}={v}\n");
            let mut n = rest.len() + 1;
            loop {
                let full = format!("{n}{rest}");
                if full.len() == n {
                    body.extend_from_slice(full.as_bytes());
                    break;
                }
                n = full.len();
            }
        }
        let h = Self::header(b"PaxHeader", b'x', 0o644, body.len(), b"", 0, 0, false);
        self.push(h, &body);
    }

    fn gnu_long(&mut self, kind: u8, value: &[u8]) {
        let mut body = value.to_vec();
        body.push(0);
        let mut h = Self::header(b"././@LongLink", kind, 0o644, body.len(), b"", 0, 0, false);
        h[257..265].copy_from_slice(b"ustar  \0");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        self.push(h, &body);
    }

    fn finish(mut self) -> Vec<u8> {
        self.data.extend_from_slice(&[0u8; 1024]);
        self.data
    }
}

/// Randomized options over random trees. Run with
/// `STRESS_SEEDS=500 cargo test --release --test compat -- --ignored`.
#[test]
#[ignore]
fn stress() {
    if !have_reference() {
        return;
    }
    let seeds: u64 = std::env::var("STRESS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let t = TempDir::new();
    let mut compared = 0;
    for seed in 0..seeds {
        let mut rng = Rng::new(seed ^ 0xA5A5);
        let root = t.path().join(format!("s{seed}"));
        gen_tree(
            &root,
            seed + 10_000,
            Profile {
                max_depth: rng.range(1, 5) as u32,
                max_entries: rng.range(1, 40),
                max_file: *rng.pick(&[10_000usize, 300_000, 3_000_000]),
                ..Profile::default()
            },
        );
        let r = s(&root);
        let mut args: Vec<String> =
            vec!["-B".into(), rng.pick(&["1024", "2048", "4096"]).to_string()];
        if rng.chance(70) {
            args.push("-f".into());
        }
        match rng.below(4) {
            0 => args.extend(["-r".into(), format!("+{}", rng.range(300, 5000))]),
            1 => args.extend(["-r".into(), format!("+{}%", rng.range(20, 300))]),
            2 => args.extend(["-b".into(), rng.range(5_000, 200_000).to_string()]),
            _ => args.extend([
                "-r".into(),
                "+3000".into(),
                "-N".into(),
                rng.range(100, 20_000).to_string(),
            ]),
        }
        for (pct, flag) in [(20, "-z"), (15, "-P")] {
            if rng.chance(pct) {
                args.push(flag.into());
            }
        }
        if rng.chance(15) {
            args.extend(["-U".into(), rng.range(0, 70_000).to_string()]);
        }
        if rng.chance(10) {
            args.extend(["-m".into(), rng.range(0, 30).to_string()]);
        }
        if rng.chance(10) {
            args.extend(["-e".into(), rng.range(1, 255).to_string()]);
        }
        args.extend(["-d".into(), r]);
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        match compare(t.path(), &a) {
            Ok(CompareOutcome::Equivalent(_)) => compared += 1,
            Ok(_) => {}
            Err(e) => panic!("seed {seed}: {e}"),
        }
        let _ = fs::remove_dir_all(&root);
    }
    eprintln!("{compared} of {seeds} configurations matched xgenext2fs' content");
    assert!(compared > seeds / 2);
}
