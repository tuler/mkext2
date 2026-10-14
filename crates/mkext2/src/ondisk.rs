//! On-disk structures of the ext2 format (revision 1, 128-byte inodes),
//! as documented in the Linux kernel's `Documentation/filesystems/ext2.rst`
//! and Dave Poirier's "The Second Extended File System".
//!
//! Every structure is kept as its raw little-endian bytes, with accessors
//! for the fields used here.

/// File type mask and types (`i_mode`).
pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;
/// Permission bits, including set-uid, set-gid and sticky.
pub const S_IMASK: u32 = 0o7777;

pub const EXT2_MAGIC: u16 = 0xEF53;
/// The root directory's inode.
pub const EXT2_ROOT_INO: u32 = 2;
/// First inode that is not reserved (revision 0, and what mkext2 writes).
pub const EXT2_FIRST_INO: u32 = 11;

pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const GROUP_DESC_SIZE: usize = 32;
pub const INODE_SIZE: usize = 128;
/// Size of a directory entry without its name.
pub const DIRENT_HEADER: usize = 8;
/// Longest name in a directory entry.
pub const NAME_MAX: usize = 255;
/// Direct block pointers in an inode; then single, double, triple.
pub const NDIR_BLOCKS: usize = 12;
pub const IND_BLOCK: usize = 12;
pub const DIND_BLOCK: usize = 13;
pub const TIND_BLOCK: usize = 14;
pub const N_BLOCKS: usize = 15;
/// Symbolic links shorter than this are stored in the inode.
pub const FAST_SYMLINK_MAX: usize = 60;

pub const FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
pub const FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;

pub const STATE_CLEAN: u16 = 1;
pub const ERRORS_CONTINUE: u16 = 1;
pub const DYNAMIC_REV: u32 = 1;

/// Creator OS values.
pub const OS_LINUX: u32 = 0;
pub const OS_HURD: u32 = 1;
pub const OS_MASIX: u32 = 2;
pub const OS_FREEBSD: u32 = 3;
pub const OS_LITES: u32 = 4;

/// Directory entry file types (`filetype` feature).
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SOCK: u8 = 6;
pub const FT_SYMLINK: u8 = 7;

/// The directory entry type for a file type (`S_IFMT` bits).
pub fn dirent_file_type(file_type: u32) -> u8 {
    match file_type {
        S_IFREG => FT_REG_FILE,
        S_IFDIR => FT_DIR,
        S_IFCHR => FT_CHRDEV,
        S_IFBLK => FT_BLKDEV,
        S_IFIFO => FT_FIFO,
        S_IFSOCK => FT_SOCK,
        S_IFLNK => FT_SYMLINK,
        _ => FT_UNKNOWN,
    }
}

#[inline]
pub fn rd16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
pub fn rd32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn wr16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn wr32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Rounded length of a directory entry with a name of `name_len` bytes.
#[inline]
pub fn dirent_len(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len + 3) & !3
}

/// Write a directory entry header and name at `off`.
pub fn put_dirent(b: &mut [u8], off: usize, inode: u32, rec_len: usize, ftype: u8, name: &[u8]) {
    wr32(b, off, inode);
    wr16(b, off + 4, rec_len as u16);
    b[off + 6] = name.len() as u8;
    b[off + 7] = ftype;
    b[off + DIRENT_HEADER..off + DIRENT_HEADER + name.len()].copy_from_slice(name);
}

macro_rules! field32 {
    ($get:ident, $set:ident, $off:expr) => {
        #[inline]
        pub fn $get(&self) -> u32 {
            rd32(&self.0, $off)
        }
        #[inline]
        pub fn $set(&mut self, v: u32) {
            wr32(&mut self.0, $off, v)
        }
    };
}

macro_rules! field16 {
    ($get:ident, $set:ident, $off:expr) => {
        #[inline]
        pub fn $get(&self) -> u16 {
            rd16(&self.0, $off)
        }
        #[inline]
        pub fn $set(&mut self, v: u16) {
            wr16(&mut self.0, $off, v)
        }
    };
}

/// The superblock (1024 bytes at byte offset 1024, copies in some groups).
#[derive(Clone)]
pub struct Superblock(pub [u8; SUPERBLOCK_SIZE]);

impl Default for Superblock {
    fn default() -> Self {
        Superblock([0; SUPERBLOCK_SIZE])
    }
}

impl std::fmt::Debug for Superblock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Superblock")
            .field("blocks_count", &self.blocks_count())
            .field("inodes_count", &self.inodes_count())
            .field("block_size", &self.block_size())
            .finish_non_exhaustive()
    }
}

impl Superblock {
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut s = Superblock::default();
        s.0.copy_from_slice(&b[..SUPERBLOCK_SIZE]);
        s
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    field32!(inodes_count, set_inodes_count, 0);
    field32!(blocks_count, set_blocks_count, 4);
    field32!(r_blocks_count, set_r_blocks_count, 8);
    field32!(free_blocks_count, set_free_blocks_count, 12);
    field32!(free_inodes_count, set_free_inodes_count, 16);
    field32!(first_data_block, set_first_data_block, 20);
    field32!(log_block_size, set_log_block_size, 24);
    field32!(log_frag_size, set_log_frag_size, 28);
    field32!(blocks_per_group, set_blocks_per_group, 32);
    field32!(frags_per_group, set_frags_per_group, 36);
    field32!(inodes_per_group, set_inodes_per_group, 40);
    field32!(mtime, set_mtime, 44);
    field32!(wtime, set_wtime, 48);
    field16!(mnt_count, set_mnt_count, 52);
    field16!(max_mnt_count, set_max_mnt_count, 54);
    field16!(magic, set_magic, 56);
    field16!(state, set_state, 58);
    field16!(errors, set_errors, 60);
    field16!(minor_rev_level, set_minor_rev_level, 62);
    field32!(lastcheck, set_lastcheck, 64);
    field32!(checkinterval, set_checkinterval, 68);
    field32!(creator_os, set_creator_os, 72);
    field32!(rev_level, set_rev_level, 76);
    field16!(def_resuid, set_def_resuid, 80);
    field16!(def_resgid, set_def_resgid, 82);
    field32!(first_ino_raw, set_first_ino, 84);
    field16!(inode_size_raw, set_inode_size, 88);
    field16!(block_group_nr, set_block_group_nr, 90);
    field32!(feature_compat, set_feature_compat, 92);
    field32!(feature_incompat, set_feature_incompat, 96);
    field32!(feature_ro_compat, set_feature_ro_compat, 100);

    /// First non-reserved inode (fixed at 11 in revision 0).
    pub fn first_ino(&self) -> u32 {
        if self.rev_level() >= DYNAMIC_REV {
            self.first_ino_raw()
        } else {
            EXT2_FIRST_INO
        }
    }

    /// Inode size (fixed at 128 in revision 0).
    pub fn inode_size(&self) -> u16 {
        if self.rev_level() >= DYNAMIC_REV {
            self.inode_size_raw()
        } else {
            INODE_SIZE as u16
        }
    }

    pub fn block_size(&self) -> u32 {
        1024u32 << self.log_block_size().min(16)
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u32 {
        let bpg = self.blocks_per_group().max(1);
        self.blocks_count()
            .saturating_sub(self.first_data_block())
            .div_ceil(bpg)
    }

    pub fn uuid(&self) -> &[u8] {
        &self.0[104..120]
    }

    /// The volume label, zero padded to 16 bytes.
    pub fn volume_name(&self) -> &[u8] {
        &self.0[120..136]
    }

    pub fn set_volume_name(&mut self, label: &[u8]) {
        let n = label.len().min(16);
        self.0[120..136].fill(0);
        self.0[120..120 + n].copy_from_slice(&label[..n]);
    }
}

/// A block group descriptor (32 bytes).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct GroupDesc(pub [u8; GROUP_DESC_SIZE]);

impl GroupDesc {
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut g = GroupDesc::default();
        g.0.copy_from_slice(&b[..GROUP_DESC_SIZE]);
        g
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    field32!(block_bitmap, set_block_bitmap, 0);
    field32!(inode_bitmap, set_inode_bitmap, 4);
    field32!(inode_table, set_inode_table, 8);
    field16!(free_blocks_count, set_free_blocks_count, 12);
    field16!(free_inodes_count, set_free_inodes_count, 14);
    field16!(used_dirs_count, set_used_dirs_count, 16);
}

/// An inode (the 128 bytes of the original format).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Inode(pub [u8; INODE_SIZE]);

impl Default for Inode {
    fn default() -> Self {
        Inode([0; INODE_SIZE])
    }
}

impl std::fmt::Debug for Inode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inode")
            .field("mode", &format_args!("{:o}", self.mode()))
            .field("size", &self.size64())
            .field("links", &self.links_count())
            .finish_non_exhaustive()
    }
}

impl Inode {
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut i = Inode::default();
        i.0.copy_from_slice(&b[..INODE_SIZE]);
        i
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    field16!(mode, set_mode, 0);
    // Low 16 bits of the owner.
    field16!(uid, set_uid_low, 2);
    field32!(size, set_size_low, 4);
    field32!(atime, set_atime, 8);
    field32!(ctime, set_ctime, 12);
    field32!(mtime, set_mtime, 16);
    field32!(dtime, set_dtime, 20);
    // Low 16 bits of the group.
    field16!(gid, set_gid_low, 24);
    field16!(links_count, set_links_count, 26);
    // Allocated space, in 512-byte units.
    field32!(blocks, set_blocks, 28);
    field32!(flags, set_flags, 32);
    field32!(size_high, set_size_high, 108);
    field16!(uid_high, set_uid_high, 120);
    field16!(gid_high, set_gid_high, 122);

    pub fn file_type(&self) -> u32 {
        u32::from(self.mode()) & S_IFMT
    }

    /// The size, including the high 32 bits of regular files.
    pub fn size64(&self) -> u64 {
        let low = u64::from(self.size());
        if self.file_type() == S_IFREG {
            low | (u64::from(self.size_high()) << 32)
        } else {
            low
        }
    }

    pub fn set_size64(&mut self, size: u64) {
        self.set_size_low(size as u32);
        self.set_size_high((size >> 32) as u32);
    }

    /// The full 32-bit owner.
    pub fn uid32(&self) -> u32 {
        u32::from(self.uid()) | (u32::from(self.uid_high()) << 16)
    }

    /// The full 32-bit group.
    pub fn gid32(&self) -> u32 {
        u32::from(self.gid()) | (u32::from(self.gid_high()) << 16)
    }

    pub fn set_uid(&mut self, uid: u32) {
        self.set_uid_low(uid as u16);
        self.set_uid_high((uid >> 16) as u16);
    }

    pub fn set_gid(&mut self, gid: u32) {
        self.set_gid_low(gid as u16);
        self.set_gid_high((gid >> 16) as u16);
    }

    /// Block pointer `i` (0..15).
    #[inline]
    pub fn block(&self, i: usize) -> u32 {
        rd32(&self.0, 40 + 4 * i)
    }

    #[inline]
    pub fn set_block(&mut self, i: usize, b: u32) {
        wr32(&mut self.0, 40 + 4 * i, b)
    }

    /// The 60 bytes of block pointers (a fast symlink's target).
    pub fn block_bytes(&self) -> &[u8] {
        &self.0[40..100]
    }

    pub fn block_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.0[40..100]
    }
}

/// Does group `g` hold a superblock backup under `sparse_super` (groups 0,
/// 1 and powers of 3, 5 and 7)?
pub fn has_superblock(g: u32) -> bool {
    if g <= 1 {
        return true;
    }
    [3u32, 5, 7].into_iter().any(|base| {
        let mut p = base;
        while p < g {
            match p.checked_mul(base) {
                Some(n) => p = n,
                None => return false,
            }
        }
        p == g
    })
}

/// Device number encoding in an inode's first two block pointers: the old
/// 16-bit form when both numbers are below 256, the new 32-bit form
/// otherwise (as Linux does).
pub fn encode_device(major: u32, minor: u32) -> (u32, u32) {
    if major < 256 && minor < 256 {
        ((major << 8) | minor, 0)
    } else {
        (
            0,
            (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_groups() {
        let g: Vec<u32> = (0..100).filter(|&g| has_superblock(g)).collect();
        assert_eq!(g, vec![0, 1, 3, 5, 7, 9, 25, 27, 49, 81]);
    }

    #[test]
    fn devices() {
        assert_eq!(encode_device(5, 1), (0x0501, 0));
        let (a, b) = encode_device(1, 300);
        assert_eq!(a, 0);
        let mut ino = Inode::default();
        ino.set_block(0, a);
        ino.set_block(1, b);
        assert_eq!(crate::reader::decode_device(&ino), (1, 300));
    }

    #[test]
    fn owners() {
        let mut ino = Inode::default();
        ino.set_uid(100_000);
        assert_eq!(ino.uid(), (100_000 & 0xffff) as u16);
        assert_eq!(ino.uid32(), 100_000);
    }
}
