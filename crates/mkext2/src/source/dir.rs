//! Host directory traversal and file metadata.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::ondisk::*;

/// File metadata, as read with `lstat`.
#[derive(Clone, Debug)]
pub(crate) struct Meta {
    /// File type bits (`S_IFMT`), 0 when unknown.
    pub kind: u32,
    /// Permission bits (`07777`).
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub nlink: u64,
    /// Device and inode number: the file's identity, for hard links.
    pub dev: u64,
    pub ino: u64,
    pub ctime: i64,
    pub mtime: i64,
    pub rdev_major: u32,
    pub rdev_minor: u32,
}

#[cfg(target_os = "linux")]
fn split_dev(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn split_dev(dev: u64) -> (u32, u32) {
    // BSD/macOS encoding
    (((dev >> 24) & 0xff) as u32, (dev & 0xff_ffff) as u32)
}

#[cfg(unix)]
pub(crate) fn lstat(path: &Path) -> io::Result<Meta> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = fs::symlink_metadata(path)?;
    let ft = m.file_type();
    let kind = if ft.is_dir() {
        S_IFDIR
    } else if ft.is_file() {
        S_IFREG
    } else if ft.is_symlink() {
        S_IFLNK
    } else if ft.is_char_device() {
        S_IFCHR
    } else if ft.is_block_device() {
        S_IFBLK
    } else if ft.is_fifo() {
        S_IFIFO
    } else if ft.is_socket() {
        S_IFSOCK
    } else {
        0
    };
    let (rdev_major, rdev_minor) = split_dev(m.rdev());
    Ok(Meta {
        kind,
        perm: m.mode() & S_IMASK,
        uid: m.uid(),
        gid: m.gid(),
        size: m.size(),
        nlink: m.nlink(),
        dev: m.dev(),
        ino: m.ino(),
        ctime: m.ctime(),
        mtime: m.mtime(),
        rdev_major,
        rdev_minor,
    })
}

#[cfg(not(unix))]
pub(crate) fn lstat(path: &Path) -> io::Result<Meta> {
    let m = fs::symlink_metadata(path)?;
    let ft = m.file_type();
    let kind = if ft.is_dir() {
        S_IFDIR
    } else if ft.is_symlink() {
        S_IFLNK
    } else if ft.is_file() {
        S_IFREG
    } else {
        0
    };
    let perm = if ft.is_dir() {
        0o755
    } else if m.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    let secs = |t: io::Result<std::time::SystemTime>| -> i64 {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    };
    let mtime = secs(m.modified());
    Ok(Meta {
        kind,
        perm,
        uid: 0,
        gid: 0,
        size: m.len(),
        nlink: 1,
        dev: 0,
        ino: 0,
        ctime: mtime,
        mtime,
        rdev_major: 0,
        rdev_minor: 0,
    })
}

/// Name bytes of a directory entry.
#[cfg(unix)]
pub(crate) fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(crate) fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// Directory entries (without `.` and `..`) sorted by byte value, like
/// `scandir(..., alphasort)` in the C locale.
pub(crate) fn read_sorted(dir: &Path) -> io::Result<Vec<(Vec<u8>, PathBuf)>> {
    let mut v = Vec::new();
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name();
        let bytes = name_bytes(&name);
        if bytes == b"." || bytes == b".." {
            continue;
        }
        v.push((bytes, ent.path()));
    }
    v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

/// Read a symlink's target bytes.
#[cfg(unix)]
pub(crate) fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
pub(crate) fn read_link(path: &Path) -> io::Result<Vec<u8>> {
    Ok(fs::read_link(path)?
        .to_string_lossy()
        .replace('\\', "/")
        .into_bytes())
}

pub(crate) fn open(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Whether this process may read a file with this metadata, judging by
/// its owner and permission bits (opening every file twice would be
/// slow).
#[cfg(unix)]
pub(crate) fn may_read(m: &Meta) -> bool {
    // SAFETY: these calls have no preconditions.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return true;
    }
    if m.uid == euid {
        return m.perm & 0o400 != 0;
    }
    if in_group(m.gid) {
        return m.perm & 0o040 != 0;
    }
    m.perm & 0o004 != 0
}

#[cfg(unix)]
fn in_group(gid: u32) -> bool {
    use std::sync::OnceLock;
    static GROUPS: OnceLock<Vec<u32>> = OnceLock::new();
    let groups = GROUPS.get_or_init(|| {
        // SAFETY: getgroups with a correctly sized buffer.
        unsafe {
            let n = libc::getgroups(0, std::ptr::null_mut());
            let mut v = vec![0 as libc::gid_t; n.max(0) as usize];
            let n = libc::getgroups(v.len() as libc::c_int, v.as_mut_ptr());
            v.truncate(n.max(0) as usize);
            let mut g: Vec<u32> = v;
            g.push(libc::getegid());
            g
        }
    });
    groups.contains(&gid)
}

#[cfg(not(unix))]
pub(crate) fn may_read(_m: &Meta) -> bool {
    true
}
