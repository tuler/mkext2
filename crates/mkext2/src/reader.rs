//! A small, independent, read-only ext2 reader and consistency checker.
//!
//! It shares no code with the writer beyond the on-disk field accessors, so
//! it can be used to verify generated images: list and extract their
//! content and check the allocation metadata (a minimal `e2fsck -n`).

use std::collections::{BTreeMap, HashMap};

use crate::error::{Error, Result};
use crate::ondisk::*;

const FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
const FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const SUPPORTED_INCOMPAT: u32 = FEATURE_INCOMPAT_FILETYPE;
const SUPPORTED_RO_COMPAT: u32 = FEATURE_RO_COMPAT_SPARSE_SUPER | FEATURE_RO_COMPAT_LARGE_FILE;

/// Directory entry file types (with the `filetype` feature).
fn dirent_type(mode_type: u32) -> u8 {
    match mode_type {
        S_IFREG => 1,
        S_IFDIR => 2,
        S_IFCHR => 3,
        S_IFBLK => 4,
        S_IFIFO => 5,
        S_IFSOCK => 6,
        S_IFLNK => 7,
        _ => 0,
    }
}

/// Does group `g` hold a superblock backup under `sparse_super`?
pub fn sparse_super_group(g: u32) -> bool {
    if g <= 1 {
        return true;
    }
    for base in [3u32, 5, 7] {
        let mut p = base;
        while p < g {
            match p.checked_mul(base) {
                Some(n) => p = n,
                None => break,
            }
        }
        if p == g {
            return true;
        }
    }
    false
}

/// Device numbers stored in an inode's block area, in either Linux
/// encoding (old 16-bit in the first word, new 32-bit in the second).
pub fn decode_device(ino: &Inode) -> (u32, u32) {
    let old = ino.block(0);
    if old != 0 {
        ((old >> 8) & 0xff, old & 0xff)
    } else {
        let new = ino.block(1);
        ((new & 0xfff00) >> 8, (new & 0xff) | ((new >> 12) & 0xfff00))
    }
}

/// A directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub inode: u32,
    /// File type byte (0 without the `filetype` feature).
    pub file_type: u8,
}

/// One node of the tree, as returned by [`Image::walk`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub path: Vec<u8>,
    pub inode: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub links: u16,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    /// Device major and minor numbers (devices only).
    pub rdev: (u32, u32),
}

/// Blocks of an inode: (logical index, block) for data, then the
/// indirection blocks.
pub type InodeBlocks = (Vec<(u64, u32)>, Vec<u32>);

/// A read-only view of an ext2 image held in memory.
pub struct Image {
    data: Vec<u8>,
    sb: Superblock,
    bs: u32,
    gds: Vec<GroupDesc>,
    inode_size: usize,
}

fn corrupt(msg: impl Into<String>) -> Error {
    Error::fatal(msg.into())
}

impl Image {
    /// Parse an image.
    pub fn new(data: Vec<u8>) -> Result<Self> {
        if data.len() < 2048 {
            return Err(corrupt("image too small"));
        }
        let sb = Superblock::from_bytes(&data[1024..2048]);
        if sb.magic() != EXT2_MAGIC {
            return Err(corrupt("bad magic number"));
        }
        if sb.log_block_size() > 6 {
            return Err(corrupt("bad block size"));
        }
        let bs = sb.block_size();
        if sb.blocks_per_group() == 0 || sb.inodes_per_group() == 0 {
            return Err(corrupt("bad group geometry"));
        }
        let inode_size = if sb.rev_level() >= 1 {
            usize::from(sb.inode_size())
        } else {
            INODE_SIZE
        };
        if inode_size < INODE_SIZE || inode_size > bs as usize {
            return Err(corrupt("bad inode size"));
        }
        let ngroups = sb.group_count();
        let gdt = (u64::from(sb.first_data_block()) + 1) * u64::from(bs);
        let mut gds = Vec::with_capacity(ngroups as usize);
        for g in 0..u64::from(ngroups) {
            let off = (gdt + g * GROUP_DESC_SIZE as u64) as usize;
            let raw = data
                .get(off..off + GROUP_DESC_SIZE)
                .ok_or_else(|| corrupt("group descriptor table out of range"))?;
            gds.push(GroupDesc::from_bytes(raw));
        }
        Ok(Image {
            data,
            sb,
            bs,
            gds,
            inode_size,
        })
    }

    /// Read an image file.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path).map_err(|e| Error::io(path.display().to_string(), e))?;
        Image::new(data)
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    pub fn group_descriptors(&self) -> &[GroupDesc] {
        &self.gds
    }

    pub fn block_size(&self) -> u32 {
        self.bs
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// The raw bytes of block `b`.
    pub fn block(&self, b: u32) -> Result<&[u8]> {
        let start = u64::from(b) * u64::from(self.bs);
        let end = start + u64::from(self.bs);
        self.data
            .get(start as usize..end as usize)
            .ok_or_else(|| corrupt(format!("block {b} out of range")))
    }

    pub fn inode(&self, n: u32) -> Result<Inode> {
        if n == 0 || n > self.sb.inodes_count() {
            return Err(corrupt(format!("inode {n} out of range")));
        }
        let ipg = self.sb.inodes_per_group();
        let g = (n - 1) / ipg;
        let idx = (n - 1) % ipg;
        let gd = self
            .gds
            .get(g as usize)
            .ok_or_else(|| corrupt(format!("inode {n}: no group")))?;
        let off = u64::from(gd.inode_table()) * u64::from(self.bs)
            + u64::from(idx) * self.inode_size as u64;
        let raw = self
            .data
            .get(off as usize..off as usize + INODE_SIZE)
            .ok_or_else(|| corrupt(format!("inode {n} out of range")))?;
        Ok(Inode::from_bytes(raw))
    }

    fn ptr(&self, blk: u32, idx: u32) -> Result<u32> {
        let b = self.block(blk)?;
        Ok(rd32(b, 4 * idx as usize))
    }

    /// Map logical block `l` of an inode to a physical block (0 = hole).
    pub fn bmap(&self, ino: &Inode, l: u64) -> Result<u32> {
        let ppb = u64::from(self.bs / 4);
        if l < 12 {
            return Ok(ino.block(l as usize));
        }
        let mut l = l - 12;
        if l < ppb {
            let ind = ino.block(12);
            return if ind == 0 {
                Ok(0)
            } else {
                self.ptr(ind, l as u32)
            };
        }
        l -= ppb;
        if l < ppb * ppb {
            let dind = ino.block(13);
            if dind == 0 {
                return Ok(0);
            }
            let ind = self.ptr(dind, (l / ppb) as u32)?;
            return if ind == 0 {
                Ok(0)
            } else {
                self.ptr(ind, (l % ppb) as u32)
            };
        }
        l -= ppb * ppb;
        if l < ppb * ppb * ppb {
            let tind = ino.block(14);
            if tind == 0 {
                return Ok(0);
            }
            let dind = self.ptr(tind, (l / (ppb * ppb)) as u32)?;
            if dind == 0 {
                return Ok(0);
            }
            let ind = self.ptr(dind, ((l / ppb) % ppb) as u32)?;
            return if ind == 0 {
                Ok(0)
            } else {
                self.ptr(ind, (l % ppb) as u32)
            };
        }
        Err(corrupt("logical block out of range"))
    }

    /// Every block an inode owns: (data blocks by logical index, metadata blocks).
    pub fn inode_blocks(&self, ino: &Inode) -> Result<InodeBlocks> {
        let mut data = Vec::new();
        let mut meta = Vec::new();
        let t = ino.file_type();
        let fast_symlink = t == S_IFLNK && ino.blocks() == 0;
        if fast_symlink || t == S_IFCHR || t == S_IFBLK || t == S_IFIFO || t == S_IFSOCK {
            return Ok((data, meta));
        }
        let ppb = u64::from(self.bs / 4);
        for i in 0..12u64 {
            let b = ino.block(i as usize);
            if b != 0 {
                data.push((i, b));
            }
        }
        // (pointer block, depth, first logical index)
        let walk = |blk: u32,
                    depth: u32,
                    first: u64,
                    data: &mut Vec<(u64, u32)>,
                    meta: &mut Vec<u32>|
         -> Result<()> {
            fn rec(
                img: &Image,
                blk: u32,
                depth: u32,
                first: u64,
                ppb: u64,
                data: &mut Vec<(u64, u32)>,
                meta: &mut Vec<u32>,
            ) -> Result<()> {
                meta.push(blk);
                let span = ppb.pow(depth - 1);
                for i in 0..ppb {
                    let p = img.ptr(blk, i as u32)?;
                    if p == 0 {
                        continue;
                    }
                    if depth == 1 {
                        data.push((first + i, p));
                    } else {
                        rec(img, p, depth - 1, first + i * span, ppb, data, meta)?;
                    }
                }
                Ok(())
            }
            if blk != 0 {
                rec(self, blk, depth, first, ppb, data, meta)?;
            }
            Ok(())
        };
        walk(ino.block(12), 1, 12, &mut data, &mut meta)?;
        walk(ino.block(13), 2, 12 + ppb, &mut data, &mut meta)?;
        walk(ino.block(14), 3, 12 + ppb + ppb * ppb, &mut data, &mut meta)?;
        Ok((data, meta))
    }

    /// The content of a regular file or symlink.
    pub fn read_file(&self, n: u32) -> Result<Vec<u8>> {
        let ino = self.inode(n)?;
        let size = ino.size64();
        if ino.file_type() == S_IFLNK && ino.blocks() == 0 {
            return Ok(ino.block_bytes()[..(size as usize).min(60)].to_vec());
        }
        let mut out = vec![0u8; size as usize];
        let bs = u64::from(self.bs);
        let (data, _) = self.inode_blocks(&ino)?;
        for (l, b) in data {
            let start = l * bs;
            if start >= size {
                continue;
            }
            let n = (size - start).min(bs) as usize;
            out[start as usize..start as usize + n].copy_from_slice(&self.block(b)?[..n]);
        }
        Ok(out)
    }

    /// The target of a symbolic link.
    pub fn read_link(&self, n: u32) -> Result<Vec<u8>> {
        self.read_file(n)
    }

    /// Entries of a directory, in on-disk order (including `.` and `..`).
    pub fn read_dir(&self, n: u32) -> Result<Vec<DirEntry>> {
        let ino = self.inode(n)?;
        if ino.file_type() != S_IFDIR {
            return Err(corrupt(format!("inode {n} is not a directory")));
        }
        let filetype = self.sb.feature_incompat() & FEATURE_INCOMPAT_FILETYPE != 0;
        let bs = self.bs as usize;
        let nblocks = u64::from(ino.size()).div_ceil(u64::from(self.bs));
        let mut out = Vec::new();
        for l in 0..nblocks {
            let b = self.bmap(&ino, l)?;
            if b == 0 {
                return Err(corrupt(format!("directory {n} has a hole")));
            }
            let block = self.block(b)?;
            let mut off = 0usize;
            while off < bs {
                if off + DIRENT_HEADER > bs {
                    return Err(corrupt(format!("directory {n}: truncated entry")));
                }
                let child = rd32(block, off);
                let rec = rd16(block, off + 4) as usize;
                let nlen = if filetype {
                    usize::from(block[off + 6])
                } else {
                    usize::from(rd16(block, off + 6))
                };
                if rec < DIRENT_HEADER
                    || rec % 4 != 0
                    || off + rec > bs
                    || DIRENT_HEADER + nlen > rec
                {
                    return Err(corrupt(format!(
                        "directory {n}: bad entry at block {b} offset {off} (rec_len {rec}, name_len {nlen})"
                    )));
                }
                if child != 0 {
                    out.push(DirEntry {
                        name: block[off + DIRENT_HEADER..off + DIRENT_HEADER + nlen].to_vec(),
                        inode: child,
                        file_type: if filetype { block[off + 7] } else { 0 },
                    });
                }
                off += rec;
            }
        }
        Ok(out)
    }

    /// Resolve an absolute path.
    pub fn lookup(&self, path: &[u8]) -> Result<Option<u32>> {
        let mut cur = EXT2_ROOT_INO;
        for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
            let ents = self.read_dir(cur)?;
            match ents.iter().find(|e| e.name == comp) {
                Some(e) => cur = e.inode,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// Every path in the tree (depth first, directory order), excluding
    /// `.` and `..`; the root is reported as `/`.
    pub fn walk(&self) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut stack = vec![(b"/".to_vec(), EXT2_ROOT_INO, 0u32)];
        while let Some((path, n, depth)) = stack.pop() {
            if depth > 4096 {
                return Err(corrupt("directory tree too deep (loop?)"));
            }
            let ino = self.inode(n)?;
            out.push(Node {
                path: path.clone(),
                inode: n,
                mode: u32::from(ino.mode()),
                uid: u32::from(ino.uid()),
                gid: u32::from(ino.gid()),
                size: ino.size64(),
                links: ino.links_count(),
                atime: ino.atime(),
                ctime: ino.ctime(),
                mtime: ino.mtime(),
                rdev: match ino.file_type() {
                    S_IFCHR | S_IFBLK => decode_device(&ino),
                    _ => (0, 0),
                },
            });
            if ino.file_type() == S_IFDIR {
                let mut children = Vec::new();
                for e in self.read_dir(n)? {
                    if e.name == b"." || e.name == b".." {
                        continue;
                    }
                    let mut p = path.clone();
                    if p != b"/" {
                        p.push(b'/');
                    }
                    p.extend_from_slice(&e.name);
                    children.push((p, e.inode, depth + 1));
                }
                // reverse so they pop in directory order
                stack.extend(children.into_iter().rev());
            }
        }
        Ok(out)
    }

    /// Is block `b` marked in use in its group's block bitmap?
    pub fn block_used(&self, b: u32) -> Result<bool> {
        let fdb = self.sb.first_data_block();
        let bpg = self.sb.blocks_per_group();
        if b < fdb || b >= self.sb.blocks_count() {
            return Err(corrupt(format!("block {b} out of range")));
        }
        let g = (b - fdb) / bpg;
        let gd = self
            .gds
            .get(g as usize)
            .ok_or_else(|| corrupt(format!("block {b}: no group")))?;
        self.bitmap_bit(gd.block_bitmap(), (b - fdb) % bpg)
    }

    fn bitmap_bit(&self, blk: u32, idx: u32) -> Result<bool> {
        let b = self.block(blk)?;
        Ok(b[(idx / 8) as usize] & (1 << (idx % 8)) != 0)
    }

    /// Check the image for consistency; returns the problems found.
    pub fn check(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let sb = &self.sb;
        let bs = self.bs;
        let ngroups = self.gds.len() as u32;
        let bpg = sb.blocks_per_group();
        let ipg = sb.inodes_per_group();
        let fdb = sb.first_data_block();
        let total = sb.blocks_count();

        if u64::from(total) * u64::from(bs) > self.data.len() as u64 {
            problems.push(format!(
                "image is {} bytes, smaller than {} blocks",
                self.data.len(),
                total
            ));
        }
        if fdb != u32::from(bs == 1024) {
            problems.push(format!("unexpected first data block {fdb}"));
        }
        if u64::from(ipg) * u64::from(ngroups) != u64::from(sb.inodes_count()) {
            problems.push("inode count does not match groups".to_string());
        }
        if bpg > bs * 8 || ipg > bs * 8 {
            problems.push("group too large for its bitmaps".to_string());
        }
        if sb.rev_level() >= 1 {
            let incompat = sb.feature_incompat() & !SUPPORTED_INCOMPAT;
            if incompat != 0 {
                problems.push(format!("unsupported incompatible features {incompat:#x}"));
            }
            let ro = sb.feature_ro_compat() & !SUPPORTED_RO_COMPAT;
            if ro != 0 {
                problems.push(format!("unsupported read-only compatible features {ro:#x}"));
            }
        }
        // with sparse_super, the backups must be there and agree
        if sb.feature_ro_compat() & FEATURE_RO_COMPAT_SPARSE_SUPER != 0 {
            let gdt_len = ngroups as usize * GROUP_DESC_SIZE;
            let primary_gdt = (u64::from(fdb) + 1) * u64::from(bs);
            for g in 1..ngroups {
                if !sparse_super_group(g) {
                    continue;
                }
                let start = u64::from(fdb + g * bpg) * u64::from(bs);
                let Some(raw) = self
                    .data
                    .get(start as usize..start as usize + SUPERBLOCK_SIZE)
                else {
                    problems.push(format!("group {g}: backup superblock out of range"));
                    continue;
                };
                let copy = Superblock::from_bytes(raw);
                if copy.magic() != EXT2_MAGIC
                    || copy.blocks_count() != sb.blocks_count()
                    || copy.inodes_count() != sb.inodes_count()
                    || copy.blocks_per_group() != bpg
                    || copy.inodes_per_group() != ipg
                    || copy.log_block_size() != sb.log_block_size()
                {
                    problems.push(format!("group {g}: backup superblock missing or different"));
                }
                let gdt = start + u64::from(bs);
                let a = &self.data[primary_gdt as usize..primary_gdt as usize + gdt_len];
                match self.data.get(gdt as usize..gdt as usize + gdt_len) {
                    // free counts may legitimately lag in backups; compare locations
                    Some(b) => {
                        for i in 0..ngroups as usize {
                            let (x, y) = (
                                &a[i * GROUP_DESC_SIZE..i * GROUP_DESC_SIZE + 12],
                                &b[i * GROUP_DESC_SIZE..i * GROUP_DESC_SIZE + 12],
                            );
                            if x != y {
                                problems.push(format!(
                                    "group {g}: backup group descriptor {i} differs"
                                ));
                                break;
                            }
                        }
                    }
                    None => problems.push(format!("group {g}: backup descriptors out of range")),
                }
            }
        }

        // expected block usage
        let mut owner: HashMap<u32, String> = HashMap::new();
        let mut claim = |b: u32, who: String, problems: &mut Vec<String>| {
            if b < fdb || b >= total {
                problems.push(format!("{who}: block {b} out of range"));
                return;
            }
            if let Some(prev) = owner.insert(b, who.clone()) {
                problems.push(format!("block {b} claimed by {prev} and {who}"));
            }
        };
        let gdt_blocks = (ngroups * GROUP_DESC_SIZE as u32).div_ceil(bs);
        let itable_blocks = (ipg as usize * self.inode_size).div_ceil(bs as usize) as u32;
        for (g, gd) in self.gds.iter().enumerate() {
            let start = fdb + g as u32 * bpg;
            // superblock + descriptor copies: every group, or only some
            // groups with sparse_super
            let sparse = sb.feature_ro_compat() & FEATURE_RO_COMPAT_SPARSE_SUPER != 0;
            if !sparse || sparse_super_group(g as u32) {
                for b in start..start + 1 + gdt_blocks {
                    claim(
                        b,
                        format!("group {g} superblock/descriptors"),
                        &mut problems,
                    );
                }
            }
            claim(
                gd.block_bitmap(),
                format!("group {g} block bitmap"),
                &mut problems,
            );
            claim(
                gd.inode_bitmap(),
                format!("group {g} inode bitmap"),
                &mut problems,
            );
            for b in gd.inode_table()..gd.inode_table() + itable_blocks {
                claim(b, format!("group {g} inode table"), &mut problems);
            }
        }

        // walk all in-use inodes
        let mut used_inodes = vec![false; sb.inodes_count() as usize + 1];
        let mut dirs_per_group = vec![0u32; ngroups as usize];
        let mut link_refs: BTreeMap<u32, u32> = BTreeMap::new();
        let first_ino = if sb.rev_level() >= 1 {
            sb.first_ino()
        } else {
            EXT2_FIRST_INO
        };
        for n in 1..=sb.inodes_count() {
            let g = (n - 1) / ipg;
            let idx = (n - 1) % ipg;
            let allocated = self.bitmap_bit(self.gds[g as usize].inode_bitmap(), idx)?;
            if n < first_ino && n != EXT2_ROOT_INO {
                if !allocated {
                    problems.push(format!("reserved inode {n} not marked in use"));
                }
                continue;
            }
            if !allocated {
                continue;
            }
            let ino = self.inode(n)?;
            if ino.mode() == 0 || ino.links_count() == 0 {
                if n >= first_ino || n == EXT2_ROOT_INO {
                    problems.push(format!("inode {n} marked in use but empty"));
                }
                continue;
            }
            used_inodes[n as usize] = true;
            let (data, meta) = match self.inode_blocks(&ino) {
                Ok(v) => v,
                Err(e) => {
                    problems.push(format!("inode {n}: {e}"));
                    continue;
                }
            };
            for &(_, b) in &data {
                claim(b, format!("inode {n}"), &mut problems);
            }
            for &b in &meta {
                claim(b, format!("inode {n} (indirect)"), &mut problems);
            }
            let nblocks = (data.len() + meta.len()) as u64;
            if u64::from(ino.blocks()) != nblocks * u64::from(bs / 512) {
                problems.push(format!(
                    "inode {n}: i_blocks is {}, should be {}",
                    ino.blocks(),
                    nblocks * u64::from(bs / 512)
                ));
            }
            let size = ino.size64();
            if let Some(&(last, _)) = data.iter().max_by_key(|(l, _)| *l) {
                if (last + 1) * u64::from(bs) < size && ino.file_type() == S_IFDIR {
                    problems.push(format!("directory {n}: size larger than its blocks"));
                }
                if last * u64::from(bs) >= size.max(1) && ino.file_type() != S_IFDIR {
                    problems.push(format!(
                        "inode {n}: block {last} past end of file (size {size})"
                    ));
                }
            }
            match ino.file_type() {
                S_IFDIR => {
                    dirs_per_group[g as usize] += 1;
                    if u64::from(ino.size()) % u64::from(bs) != 0 {
                        problems.push(format!(
                            "directory {n}: size not a multiple of the block size"
                        ));
                    }
                    match self.read_dir(n) {
                        Ok(ents) => {
                            if ents.first().map(|e| (&e.name[..], e.inode)) != Some((&b"."[..], n))
                            {
                                problems.push(format!("directory {n}: first entry is not '.'"));
                            }
                            if ents.get(1).map(|e| &e.name[..]) != Some(&b".."[..]) {
                                problems.push(format!("directory {n}: second entry is not '..'"));
                            }
                            let mut seen = std::collections::HashSet::new();
                            let filetype = sb.feature_incompat() & FEATURE_INCOMPAT_FILETYPE != 0;
                            for e in &ents {
                                if filetype && e.inode <= sb.inodes_count() {
                                    let want = dirent_type(self.inode(e.inode)?.file_type());
                                    if e.file_type != want {
                                        problems.push(format!(
                                            "directory {n}: entry '{}' has file type {}, should be {want}",
                                            String::from_utf8_lossy(&e.name),
                                            e.file_type
                                        ));
                                    }
                                }
                                if !seen.insert(e.name.clone()) {
                                    problems.push(format!(
                                        "directory {n}: duplicate entry '{}'",
                                        String::from_utf8_lossy(&e.name)
                                    ));
                                }
                                *link_refs.entry(e.inode).or_insert(0) += 1;
                            }
                        }
                        Err(e) => problems.push(e.to_string()),
                    }
                }
                S_IFLNK => {
                    if ino.blocks() == 0 && size >= 60 {
                        problems.push(format!("symlink {n}: inline target too long"));
                    }
                    if size >= u64::from(bs) {
                        problems.push(format!(
                            "symlink {n}: target not shorter than the block size"
                        ));
                    }
                }
                S_IFREG | S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK => {}
                t => problems.push(format!("inode {n}: unknown file type {t:o}")),
            }
        }

        // link counts
        for n in 1..=sb.inodes_count() {
            if !used_inodes[n as usize] {
                if let Some(c) = link_refs.get(&n) {
                    problems.push(format!("{c} directory entries point to unused inode {n}"));
                }
                continue;
            }
            let ino = self.inode(n)?;
            let refs = link_refs.get(&n).copied().unwrap_or(0);
            if u32::from(ino.links_count()) != refs {
                problems.push(format!(
                    "inode {n}: link count is {}, should be {refs}",
                    ino.links_count()
                ));
            }
        }

        // bitmaps and counters
        let mut free_blocks_total = 0u64;
        let mut free_inodes_total = 0u64;
        for (g, gd) in self.gds.iter().enumerate() {
            let g32 = g as u32;
            let start = fdb + g32 * bpg;
            let end = (start + bpg).min(total);
            let mut free = 0u32;
            for b in start..end {
                let marked = self.bitmap_bit(gd.block_bitmap(), b - start)?;
                let used = owner.contains_key(&b);
                if marked != used {
                    problems.push(format!(
                        "block {b}: bitmap says {}, but it is {}",
                        if marked { "used" } else { "free" },
                        if used { "in use" } else { "unused" }
                    ));
                }
                if !marked {
                    free += 1;
                }
            }
            for i in (end - start)..bs * 8 {
                if !self.bitmap_bit(gd.block_bitmap(), i)? {
                    problems.push(format!(
                        "group {g}: padding bit {i} of block bitmap not set"
                    ));
                    break;
                }
            }
            if u32::from(gd.free_blocks_count()) != free {
                problems.push(format!(
                    "group {g}: free blocks count is {}, should be {free}",
                    gd.free_blocks_count()
                ));
            }
            free_blocks_total += u64::from(free);
            let mut ifree = 0u32;
            for i in 0..ipg {
                let n = g32 * ipg + i + 1;
                let marked = self.bitmap_bit(gd.inode_bitmap(), i)?;
                let used = used_inodes[n as usize] || n < first_ino;
                if marked != used {
                    problems.push(format!(
                        "inode {n}: bitmap says {}, but it is {}",
                        if marked { "used" } else { "free" },
                        if used { "in use" } else { "unused" }
                    ));
                }
                if !marked {
                    ifree += 1;
                }
            }
            if u32::from(gd.free_inodes_count()) != ifree {
                problems.push(format!(
                    "group {g}: free inodes count is {}, should be {ifree}",
                    gd.free_inodes_count()
                ));
            }
            free_inodes_total += u64::from(ifree);
            if u32::from(gd.used_dirs_count()) != dirs_per_group[g] {
                problems.push(format!(
                    "group {g}: directories count is {}, should be {}",
                    gd.used_dirs_count(),
                    dirs_per_group[g]
                ));
            }
        }
        if u64::from(sb.free_blocks_count()) != free_blocks_total {
            problems.push(format!(
                "superblock free blocks count is {}, should be {free_blocks_total}",
                sb.free_blocks_count()
            ));
        }
        if u64::from(sb.free_inodes_count()) != free_inodes_total {
            problems.push(format!(
                "superblock free inodes count is {}, should be {free_inodes_total}",
                sb.free_inodes_count()
            ));
        }
        // the root must be a directory whose '..' is itself
        match self.read_dir(EXT2_ROOT_INO) {
            Ok(ents) => {
                if !ents
                    .iter()
                    .any(|e| e.name == b".." && e.inode == EXT2_ROOT_INO)
                {
                    problems.push("root directory '..' does not point to itself".to_string());
                }
            }
            Err(e) => problems.push(format!("root directory: {e}")),
        }
        Ok(problems)
    }
}

/// What [`compare_content`] checks.
#[derive(Clone, Debug)]
pub struct CompareOptions {
    /// Compare atime, ctime and mtime.
    pub times: bool,
    /// Compare only the low 8 bits of device numbers (a tool storing just
    /// 8 bits is still considered equivalent).
    pub device_low_bits: bool,
    /// Ignore `/lost+found` (its presence and size are layout choices).
    pub ignore_lost_found: bool,
    /// Compare the volume label and creator OS.
    pub superblock: bool,
    /// Ignore symbolic links whose target is not shorter than the block
    /// size: Linux and e2fsck reject them, so a tool may leave them out.
    pub skip_invalid_symlinks: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions {
            times: true,
            device_low_bits: false,
            ignore_lost_found: true,
            superblock: true,
            skip_invalid_symlinks: true,
        }
    }
}

/// Compare what two images contain, independently of their layout: the same
/// paths, types, permissions, owners, sizes, times, contents, link targets,
/// device numbers and hard link structure. Returns the differences found.
pub fn compare_content(a: &Image, b: &Image, opts: &CompareOptions) -> Result<Vec<String>> {
    let mut diffs = Vec::new();
    if a.block_size() != b.block_size() {
        diffs.push(format!(
            "block size {} vs {}",
            a.block_size(),
            b.block_size()
        ));
    }
    if opts.superblock {
        if a.superblock().volume_name() != b.superblock().volume_name() {
            diffs.push("volume label differs".to_string());
        }
        if a.superblock().creator_os() != b.superblock().creator_os() {
            diffs.push("creator OS differs".to_string());
        }
    }
    let lost_found = |n: &Node| n.path == b"/lost+found" || n.path.starts_with(b"/lost+found/");
    let prepare = |img: &Image| -> Result<Vec<Node>> {
        let all = img.walk()?;
        let has_lf = all
            .iter()
            .any(|n| n.path == b"/lost+found" && n.mode & S_IFMT == S_IFDIR);
        let bs = u64::from(img.block_size());
        Ok(all
            .into_iter()
            .filter(|n| !(opts.ignore_lost_found && lost_found(n)))
            .filter(|n| !(opts.skip_invalid_symlinks && n.mode & S_IFMT == S_IFLNK && n.size >= bs))
            .map(|mut n| {
                // lost+found's ".." adds a link to the root
                if opts.ignore_lost_found && has_lf && n.path == b"/" {
                    n.links -= 1;
                }
                n
            })
            .collect())
    };
    let na: Vec<Node> = prepare(a)?;
    let nb: Vec<Node> = prepare(b)?;
    let ma: BTreeMap<&[u8], &Node> = na.iter().map(|n| (&n.path[..], n)).collect();
    let mb: BTreeMap<&[u8], &Node> = nb.iter().map(|n| (&n.path[..], n)).collect();
    let show = |p: &[u8]| String::from_utf8_lossy(p).into_owned();
    for p in ma.keys() {
        if !mb.contains_key(p) {
            diffs.push(format!("{}: only in the first image", show(p)));
        }
    }
    for p in mb.keys() {
        if !ma.contains_key(p) {
            diffs.push(format!("{}: only in the second image", show(p)));
        }
    }
    // hard link structure: paths sharing an inode in one image share one
    // in the other
    let mut first_a: HashMap<u32, &[u8]> = HashMap::new();
    let mut first_b: HashMap<u32, &[u8]> = HashMap::new();
    for (p, x) in &ma {
        let Some(y) = mb.get(p) else { continue };
        let what = show(p);
        let ta = x.mode & S_IFMT;
        if ta != y.mode & S_IFMT {
            diffs.push(format!("{what}: type {:o} vs {:o}", ta, y.mode & S_IFMT));
            continue;
        }
        if x.mode & 0o7777 != y.mode & 0o7777 {
            diffs.push(format!(
                "{what}: mode {:o} vs {:o}",
                x.mode & 0o7777,
                y.mode & 0o7777
            ));
        }
        if (x.uid, x.gid) != (y.uid, y.gid) {
            diffs.push(format!(
                "{what}: owner {}:{} vs {}:{}",
                x.uid, x.gid, y.uid, y.gid
            ));
        }
        if opts.times && (x.atime, x.ctime, x.mtime) != (y.atime, y.ctime, y.mtime) {
            diffs.push(format!(
                "{what}: times {}/{}/{} vs {}/{}/{}",
                x.atime, x.ctime, x.mtime, y.atime, y.ctime, y.mtime
            ));
        }
        if x.links != y.links {
            diffs.push(format!("{what}: links {} vs {}", x.links, y.links));
        }
        let fa = *first_a.entry(x.inode).or_insert(p);
        let fb = *first_b.entry(y.inode).or_insert(p);
        if fa != fb {
            diffs.push(format!(
                "{what}: hard links differ ({} vs {})",
                show(fa),
                show(fb)
            ));
        }
        match ta {
            S_IFREG | S_IFLNK => {
                if x.size != y.size {
                    diffs.push(format!("{what}: size {} vs {}", x.size, y.size));
                } else if a.read_file(x.inode)? != b.read_file(y.inode)? {
                    diffs.push(format!("{what}: content differs"));
                }
            }
            S_IFCHR | S_IFBLK => {
                let (da, db) = if opts.device_low_bits {
                    (
                        (x.rdev.0 & 0xff, x.rdev.1 & 0xff),
                        (y.rdev.0 & 0xff, y.rdev.1 & 0xff),
                    )
                } else {
                    (x.rdev, y.rdev)
                };
                if da != db {
                    diffs.push(format!("{what}: device {da:?} vs {db:?}"));
                }
            }
            _ => {}
        }
    }
    Ok(diffs)
}
