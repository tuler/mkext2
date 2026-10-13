//! Command line behavior.

mod common;

use std::fs;
use std::process::Command;

use common::*;

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(ours()).args(args).output().unwrap()
}

#[test]
fn help_and_version() {
    let h = cli(&["--help"]);
    assert!(h.status.success());
    assert!(String::from_utf8_lossy(&h.stderr).contains("--readjustment"));
    let v = cli(&["-V"]);
    assert!(v.status.success());
    assert!(String::from_utf8_lossy(&v.stdout).starts_with("mkext2 "));
}

#[test]
fn usage_errors() {
    for args in [
        vec!["--no-such-option", "x.img"],
        vec!["-B"],
        vec![],
        vec!["a.img", "b.img"],
        vec!["--squ", "1", "x.img"],
        vec!["--timestamp", "yesterday", "x.img"],
        vec!["--faketime=1", "x.img"],
    ] {
        let o = cli(&args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(!o.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn builds_and_reports() {
    let t = TempDir::new();
    let root = t.path().join("tree");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/file"), b"data").unwrap();
    fs::write(root.join("sub/file2"), b"data").unwrap();
    let img = t.path().join("out.img");
    let o = Command::new(ours())
        .args(["-B", "4096", "--timestamp", "100", "-r", "+100", "-v", "-d"])
        .arg(&root)
        .arg(&img)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.contains("sub"),
        "verbose output should list the content: {stdout}"
    );
    assert!(
        !o.stderr.is_empty(),
        "progress messages are expected on stderr"
    );
    let image = mkext2::reader::Image::open(&img).unwrap();
    assert!(image.check().unwrap().is_empty());
    assert_eq!(image.superblock().wtime(), 100);

    // a duplicate layer reports ignored entries but succeeds
    let o = Command::new(ours())
        .args(["-f", "-r", "+100", "-d"])
        .arg(&root)
        .arg("-d")
        .arg(&root)
        .arg(&img)
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("sub"),
        "duplicates are reported"
    );
}

#[test]
fn errors_exit_nonzero() {
    let t = TempDir::new();
    let img = t.path().join("out.img");
    let o = Command::new(ours())
        .args(["-f", "-d", "/definitely/not/here"])
        .arg(&img)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("/definitely/not/here"));
}
