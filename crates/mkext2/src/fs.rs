//! The image engine: geometry, allocation bitmaps, inodes and the writing
//! of file data, directories and metadata to a [`Device`].
//!
//! Layout: block groups of `8 × block size` blocks (balanced when the
//! last group would be too small for its metadata), each starting with a
//! superblock and descriptor copy when `sparse_super` asks for one, then
//! the block bitmap, the inode bitmap and the inode table. Content is
//! allocated with a single cursor that only moves forward, so files get
//! contiguous blocks, each pointer block just before the data it maps, and
//! the image is written front to back.

use std::borrow::Cow;
use std::io::{self, Read};

use crate::device::Device;
use crate::error::{Error, IoContext, Result};
use crate::hash::FxHashMap;
use crate::ondisk::*;

/// Content of a file, read in chunks.
pub(crate) trait ChunkSource {
    /// Preferred read size.
    fn chunk_size(&self, block_size: usize) -> usize;
    /// Read up to `buf.len()` bytes; 0 at the end.
    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize>;
}

/// Read until `buf` is full or the input ends; returns the bytes read.
pub(crate) fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A name or path for messages.
pub(crate) fn show(name: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(name)
}

/// Fill `buf` from `src` as far as it goes; returns the bytes read.
pub(crate) fn fill_from(src: &mut dyn ChunkSource, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = src.read_chunk(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// In-memory content.
pub(crate) struct BytesSource<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl ChunkSource for BytesSource<'_> {
    fn chunk_size(&self, _block_size: usize) -> usize {
        1 << 20
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = (self.data.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A file read from its current position, up to `limit` bytes.
pub(crate) struct FileSource<'a> {
    pub file: std::fs::File,
    pub limit: u64,
    pub done: u64,
    pub name: &'a [u8],
}

impl ChunkSource for FileSource<'_> {
    fn chunk_size(&self, _block_size: usize) -> usize {
        4 << 20
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let want = ((self.limit - self.done) as usize).min(buf.len());
        let n = read_full(&mut self.file, &mut buf[..want])
            .map_err(|e| Error::io(format!("reading {}", show(self.name)), e))?;
        self.done += n as u64;
        Ok(n)
    }
}

/// Is every byte zero?
pub(crate) fn is_zero(b: &[u8]) -> bool {
    let (head, mid, tail) = unsafe { b.align_to::<u128>() };
    head.iter().all(|&x| x == 0) && mid.iter().all(|&x| x == 0) && tail.iter().all(|&x| x == 0)
}

/// Ranges `(first block, count)` of the non-zero blocks of some content of
/// `size` bytes (the `-z` hole map).
pub(crate) fn scan_holes(src: &mut dyn ChunkSource, size: u64, bs: u32) -> Result<Vec<(u64, u64)>> {
    let bs = bs as usize;
    let chunk = (src.chunk_size(bs).max(bs) / bs) * bs;
    let mut buf = vec![0u8; chunk];
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut done = 0u64;
    let mut block = 0u64;
    while done < size {
        let want = ((size - done) as usize).min(chunk);
        let n = fill_from(src, &mut buf[..want])?;
        // a short source reads as zeros
        buf[n..want.div_ceil(bs) * bs].fill(0);
        for b in buf[..want.div_ceil(bs) * bs].chunks(bs) {
            if !is_zero(b) {
                match ranges.last_mut() {
                    Some((s, l)) if *s + *l == block => *l += 1,
                    _ => ranges.push((block, 1)),
                }
            }
            block += 1;
        }
        done += want as u64;
    }
    Ok(ranges)
}

/// Block group layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub bs: u32,
    pub blocks: u32,
    pub fdb: u32,
    pub bpg: u32,
    pub groups: u32,
    pub ipg: u32,
    pub gdt_blocks: u32,
    pub itable_blocks: u32,
    pub sparse: bool,
}

impl Geometry {
    /// The layout of a new image of `blocks` blocks with at least `inodes`
    /// inodes, if one exists.
    pub fn new(bs: u32, blocks: u64, inodes: u64) -> Option<Geometry> {
        let fdb = u32::from(bs == 1024);
        if blocks > u64::from(u32::MAX) || blocks <= u64::from(fdb) {
            return None;
        }
        let blocks = blocks as u32;
        let data = blocks - fdb;
        let max_bpg = bs * 8;
        let groups = data.div_ceil(max_bpg);
        let ipb = bs / INODE_SIZE as u32;
        let ipg = inodes
            .max(u64::from(EXT2_FIRST_INO))
            .div_ceil(u64::from(groups))
            .div_ceil(u64::from(ipb))
            * u64::from(ipb);
        if ipg > u64::from(bs * 8) || ipg * u64::from(groups) > u64::from(u32::MAX) {
            return None;
        }
        let ipg = ipg as u32;
        let gdt_blocks =
            (u64::from(groups) * GROUP_DESC_SIZE as u64).div_ceil(u64::from(bs)) as u32;
        let itable_blocks = ipg / ipb;
        let mut g = Geometry {
            bs,
            blocks,
            fdb,
            bpg: max_bpg,
            groups,
            ipg,
            gdt_blocks,
            itable_blocks,
            sparse: true,
        };
        if g.groups_fit() {
            return Some(g);
        }
        // spread the blocks evenly instead
        let bpg = data.div_ceil(groups).div_ceil(8) * 8;
        if bpg <= max_bpg && data.div_ceil(bpg) == groups {
            g.bpg = bpg;
            if g.groups_fit() {
                return Some(g);
            }
        }
        None
    }

    /// Geometry of an existing image.
    pub fn from_superblock(sb: &Superblock) -> Geometry {
        let bs = sb.block_size();
        let groups = sb.group_count();
        Geometry {
            bs,
            blocks: sb.blocks_count(),
            fdb: sb.first_data_block(),
            bpg: sb.blocks_per_group(),
            groups,
            ipg: sb.inodes_per_group(),
            gdt_blocks: (groups as usize * GROUP_DESC_SIZE).div_ceil(bs as usize) as u32,
            itable_blocks: (sb.inodes_per_group() as usize * usize::from(sb.inode_size()))
                .div_ceil(bs as usize) as u32,
            sparse: sb.feature_ro_compat() & FEATURE_RO_COMPAT_SPARSE_SUPER != 0,
        }
    }

    fn groups_fit(&self) -> bool {
        let full = 1 + self.gdt_blocks + 2 + self.itable_blocks;
        if self.groups > 1 && self.bpg < full {
            return false;
        }
        let last = self.groups - 1;
        self.group_len(last) >= self.overhead(last)
    }

    pub fn inodes(&self) -> u32 {
        self.ipg * self.groups
    }

    pub fn group_start(&self, g: u32) -> u32 {
        self.fdb + g * self.bpg
    }

    pub fn group_len(&self, g: u32) -> u32 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    pub fn has_super(&self, g: u32) -> bool {
        !self.sparse || has_superblock(g)
    }

    /// Metadata blocks at the start of group `g`.
    pub fn overhead(&self, g: u32) -> u32 {
        let sb = if self.has_super(g) {
            1 + self.gdt_blocks
        } else {
            0
        };
        sb + 2 + self.itable_blocks
    }

    /// Blocks available for content.
    pub fn data_blocks(&self) -> u64 {
        let meta: u64 = (0..self.groups).map(|g| u64::from(self.overhead(g))).sum();
        u64::from(self.blocks - self.fdb) - meta
    }

    pub fn block_bitmap(&self, g: u32) -> u32 {
        self.group_start(g)
            + if self.has_super(g) {
                1 + self.gdt_blocks
            } else {
                0
            }
    }

    pub fn inode_bitmap(&self, g: u32) -> u32 {
        self.block_bitmap(g) + 1
    }

    pub fn inode_table(&self, g: u32) -> u32 {
        self.block_bitmap(g) + 2
    }
}

/// Blocks (data and pointers) of content of `size` bytes whose non-zero
/// blocks are `present` (all of them when `None`).
pub(crate) fn count_blocks(size: u64, bs: u32, present: Option<&[(u64, u64)]>) -> u64 {
    let nblocks = size.div_ceil(u64::from(bs));
    let all = [(0u64, nblocks)];
    let ranges = present.unwrap_or(&all);
    let ppb = u64::from(bs / 4);
    let mut total = 0u64;
    // pointer blocks are identified by the first logical block they cover
    let mut last_ind: Option<u64> = None;
    let mut last_dind: Option<u64> = None;
    let mut tind = false;
    for &(start, len) in ranges {
        if len == 0 {
            continue;
        }
        total += len;
        let end = start + len; // exclusive
        if end <= NDIR_BLOCKS as u64 {
            continue;
        }
        let s = start.max(NDIR_BLOCKS as u64) - NDIR_BLOCKS as u64;
        let e = end - NDIR_BLOCKS as u64; // exclusive
        // single indirect blocks: keys k = l / ppb
        let (k1, k2) = (s / ppb, (e - 1) / ppb);
        let mut n = k2 - k1 + 1;
        if last_ind == Some(k1) {
            n -= 1;
        }
        total += n;
        last_ind = Some(k2);
        // double indirect blocks: key 0 is i_block[13], then those under
        // the triple indirect block
        if k2 >= 1 {
            let dk = |k: u64| if k <= ppb { 0 } else { 1 + (k - 1 - ppb) / ppb };
            let (d1, d2) = (dk(k1.max(1)), dk(k2));
            let mut n = d2 - d1 + 1;
            if last_dind == Some(d1) {
                n -= 1;
            }
            total += n;
            last_dind = Some(d2);
            if k2 > ppb && !tind {
                tind = true;
                total += 1;
            }
        }
    }
    total
}

/// Largest number of logical blocks a file can have.
pub(crate) fn max_file_blocks(bs: u32) -> u64 {
    let ppb = u64::from(bs / 4);
    NDIR_BLOCKS as u64 + ppb + ppb * ppb + ppb * ppb * ppb
}

/// A bit set.
#[derive(Clone, Default)]
pub(crate) struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    pub fn new(len: usize) -> Self {
        Bitmap {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        self.words[i / 64] >> (i % 64) & 1 != 0
    }

    #[inline]
    pub fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }

    #[inline]
    pub fn clear(&mut self, i: usize) {
        self.words[i / 64] &= !(1 << (i % 64));
    }

    pub fn set_range(&mut self, start: usize, len: usize) {
        let end = start + len;
        let mut i = start;
        while i < end {
            let shift = i % 64;
            let n = (64 - shift).min(end - i);
            self.words[i / 64] |= (u64::MAX >> (64 - n)) << shift;
            i += n;
        }
    }

    /// First clear bit at or after `from`.
    pub fn find_clear(&self, from: usize) -> Option<usize> {
        if from >= self.len {
            return None;
        }
        let mut w = from / 64;
        let mut word = !self.words[w] & (!0u64 << (from % 64));
        loop {
            if word != 0 {
                let i = w * 64 + word.trailing_zeros() as usize;
                return (i < self.len).then_some(i);
            }
            w += 1;
            if w >= self.words.len() {
                return None;
            }
            word = !self.words[w];
        }
    }

    /// Number of consecutive clear bits starting at `from`, at most `max`.
    pub fn clear_run(&self, from: usize, max: usize) -> usize {
        let end = from.saturating_add(max).min(self.len);
        let mut i = from;
        while i < end {
            let shift = i % 64;
            let n = (64 - shift).min(end - i);
            let clear = (self.words[i / 64] >> shift).trailing_zeros() as usize;
            if clear < n {
                return i - from + clear;
            }
            i += n;
        }
        i - from
    }

    pub fn count_set(&self, start: usize, len: usize) -> usize {
        let mut n = 0;
        let mut i = start;
        let end = start + len;
        while i < end && i % 64 != 0 {
            n += usize::from(self.get(i));
            i += 1;
        }
        while i + 64 <= end {
            n += self.words[i / 64].count_ones() as usize;
            i += 64;
        }
        while i < end {
            n += usize::from(self.get(i));
            i += 1;
        }
        n
    }

    /// The bytes of bits `start..start + len` (`start` a multiple of 8),
    /// into `out` (which is zeroed first).
    pub fn copy_bytes(&self, start: usize, len: usize, out: &mut [u8]) {
        out.fill(0);
        for (k, byte) in out.iter_mut().take(len.div_ceil(8)).enumerate() {
            let bit = start + k * 8;
            *byte = (self.words[bit / 64] >> (bit % 64)) as u8;
        }
        if len % 8 != 0 {
            out[len / 8] &= u8::MAX >> (8 - len % 8);
        }
    }
}

/// Where a file's blocks go, computed before its data is written.
pub(crate) struct DataLayout {
    pub i_block: [u32; N_BLOCKS],
    /// Data and pointer blocks.
    pub nblocks: u64,
    ops: Vec<Op>,
    ptrs: Vec<Vec<u32>>,
    /// Physical block of each logical block (0 for holes), when asked for.
    pub map: Option<Vec<u32>>,
}

enum Op {
    Ptr { phys: u32, idx: usize },
    Data { logical: u64, phys: u32, len: u32 },
}

/// Settings of a new image.
pub(crate) struct NewImage<'a> {
    pub geo: Geometry,
    pub reserved: u32,
    pub time: u32,
    pub creator_os: u32,
    pub label: Option<&'a [u8]>,
}

/// An image being written.
pub(crate) struct Fs<'d> {
    pub dev: &'d mut dyn Device,
    pub geo: Geometry,
    pub sb: Superblock,
    gds: Vec<GroupDesc>,
    blocks: Bitmap,
    inodes: Bitmap,
    deferred_free: Vec<u32>,
    cursor: usize,
    icursor: usize,
    new_dirs: Vec<u32>,
    /// Inodes to write.
    pub table: FxHashMap<u32, Inode>,
    pub existing: bool,
    pub filetype: bool,
    buf: Vec<u8>,
}

impl<'d> Fs<'d> {
    /// Start a new image on `dev` (which is cleared).
    pub fn create(dev: &'d mut dyn Device, p: NewImage<'_>) -> Result<Fs<'d>> {
        let geo = p.geo;
        let size = u64::from(geo.blocks) * u64::from(geo.bs);
        dev.set_len(0).ctx(|| "clearing the image".to_string())?;
        dev.set_len(size).ctx(|| "sizing the image".to_string())?;
        let mut sb = Superblock::default();
        sb.set_inodes_count(geo.inodes());
        sb.set_blocks_count(geo.blocks);
        sb.set_r_blocks_count(p.reserved);
        sb.set_first_data_block(geo.fdb);
        let log = (geo.bs / 1024).trailing_zeros();
        sb.set_log_block_size(log);
        sb.set_log_frag_size(log);
        sb.set_blocks_per_group(geo.bpg);
        sb.set_frags_per_group(geo.bpg);
        sb.set_inodes_per_group(geo.ipg);
        sb.set_wtime(p.time);
        sb.set_lastcheck(p.time);
        sb.set_mnt_count(0);
        sb.set_max_mnt_count(0xFFFF);
        sb.set_magic(EXT2_MAGIC);
        sb.set_state(STATE_CLEAN);
        sb.set_errors(ERRORS_CONTINUE);
        sb.set_creator_os(p.creator_os);
        sb.set_rev_level(DYNAMIC_REV);
        sb.set_first_ino(EXT2_FIRST_INO);
        sb.set_inode_size(INODE_SIZE as u16);
        sb.set_feature_incompat(FEATURE_INCOMPAT_FILETYPE);
        sb.set_feature_ro_compat(FEATURE_RO_COMPAT_SPARSE_SUPER);
        if let Some(label) = p.label {
            sb.set_volume_name(label);
        }
        let mut blocks = Bitmap::new((geo.blocks - geo.fdb) as usize);
        let mut gds = Vec::with_capacity(geo.groups as usize);
        for g in 0..geo.groups {
            let mut gd = GroupDesc::default();
            gd.set_block_bitmap(geo.block_bitmap(g));
            gd.set_inode_bitmap(geo.inode_bitmap(g));
            gd.set_inode_table(geo.inode_table(g));
            gds.push(gd);
            let start = (geo.group_start(g) - geo.fdb) as usize;
            blocks.set_range(start, geo.overhead(g) as usize);
        }
        let mut inodes = Bitmap::new(geo.inodes() as usize);
        inodes.set_range(0, (EXT2_FIRST_INO - 1) as usize);
        let mut new_dirs = vec![0u32; geo.groups as usize];
        new_dirs[0] = 1; // the root
        Ok(Fs {
            dev,
            geo,
            sb,
            gds,
            blocks,
            inodes,
            deferred_free: Vec::new(),
            cursor: 0,
            icursor: 0,
            new_dirs,
            table: FxHashMap::default(),
            existing: false,
            filetype: true,
            buf: Vec::new(),
        })
    }

    /// Open the image already on `dev`.
    pub fn open(dev: &'d mut dyn Device) -> Result<Fs<'d>> {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(SUPERBLOCK_OFFSET, &mut raw)
            .ctx(|| "reading the starting image".to_string())?;
        let sb = Superblock::from_bytes(&raw);
        let bad = |m: &str| Error::fatal(format!("starting image: {m}"));
        if sb.magic() != EXT2_MAGIC {
            return Err(bad("not an ext2 filesystem"));
        }
        if sb.rev_level() > DYNAMIC_REV {
            return Err(bad("unsupported revision"));
        }
        if sb.log_block_size() > 2 {
            return Err(bad("unsupported block size"));
        }
        if usize::from(sb.inode_size()) != INODE_SIZE {
            return Err(bad("unsupported inode size"));
        }
        if sb.feature_compat() != 0
            || sb.feature_incompat() & !FEATURE_INCOMPAT_FILETYPE != 0
            || sb.feature_ro_compat()
                & !(FEATURE_RO_COMPAT_SPARSE_SUPER | FEATURE_RO_COMPAT_LARGE_FILE)
                != 0
        {
            return Err(bad(
                "unsupported features (only filetype, sparse_super and large_file are)",
            ));
        }
        let bs = sb.block_size();
        let inode_count = u64::from(sb.group_count()) * u64::from(sb.inodes_per_group());
        if sb.blocks_per_group() == 0
            || sb.inodes_per_group() == 0
            || sb.blocks_per_group() > bs * 8
            || sb.inodes_per_group() > bs * 8
            || sb.blocks_per_group() % 8 != 0
            || sb.inodes_per_group() % 8 != 0
            || sb.first_data_block() != u32::from(bs == 1024)
            || sb.blocks_count() <= sb.first_data_block()
            || inode_count != u64::from(sb.inodes_count())
            || u64::from(sb.blocks_count()) * u64::from(bs) > dev.size()?
        {
            return Err(bad("inconsistent geometry"));
        }
        let geo = Geometry::from_superblock(&sb);
        let gdt_end =
            u64::from(geo.fdb + 1) * u64::from(bs) + u64::from(geo.groups) * GROUP_DESC_SIZE as u64;
        if gdt_end > u64::from(geo.blocks) * u64::from(bs) {
            return Err(bad("group descriptor table out of range"));
        }
        let mut gdt = vec![0u8; geo.groups as usize * GROUP_DESC_SIZE];
        dev.read_at(u64::from(geo.fdb + 1) * u64::from(bs), &mut gdt)?;
        let gds: Vec<GroupDesc> = gdt
            .chunks(GROUP_DESC_SIZE)
            .map(GroupDesc::from_bytes)
            .collect();
        let mut blocks = Bitmap::new((geo.blocks - geo.fdb) as usize);
        let mut inodes = Bitmap::new(geo.inodes() as usize);
        let mut buf = vec![0u8; bs as usize];
        for (g, gd) in gds.iter().enumerate() {
            let g32 = g as u32;
            for (bitmap, blk, start, len) in [
                (
                    &mut blocks,
                    gd.block_bitmap(),
                    (geo.group_start(g32) - geo.fdb) as usize,
                    geo.group_len(g32) as usize,
                ),
                (
                    &mut inodes,
                    gd.inode_bitmap(),
                    g * geo.ipg as usize,
                    geo.ipg as usize,
                ),
            ] {
                if blk >= geo.blocks {
                    return Err(bad("bitmap out of range"));
                }
                dev.read_at(u64::from(blk) * u64::from(bs), &mut buf)?;
                for k in 0..len {
                    if buf[k / 8] >> (k % 8) & 1 != 0 {
                        bitmap.set(start + k);
                    }
                }
            }
        }
        let filetype = sb.feature_incompat() & FEATURE_INCOMPAT_FILETYPE != 0;
        let groups = geo.groups as usize;
        Ok(Fs {
            dev,
            geo,
            sb,
            gds,
            blocks,
            inodes,
            deferred_free: Vec::new(),
            cursor: 0,
            icursor: 0,
            new_dirs: vec![0; groups],
            table: FxHashMap::default(),
            existing: true,
            filetype,
            buf: Vec::new(),
        })
    }

    pub fn bs(&self) -> u32 {
        self.geo.bs
    }

    pub fn free_blocks(&self) -> u64 {
        let n = self.blocks.len;
        (n - self.blocks.count_set(0, n)) as u64
    }

    pub fn free_inodes(&self) -> u64 {
        let n = self.inodes.len;
        (n - self.inodes.count_set(0, n)) as u64
    }

    /// Mark blocks to be freed once the image is complete (they are not
    /// reused meanwhile, so an interrupted update leaves the image intact).
    pub fn free_later(&mut self, blocks: &[u32]) {
        self.deferred_free.extend_from_slice(blocks);
    }

    pub fn alloc_inode(&mut self, dir: bool) -> Result<u32> {
        let i = self
            .inodes
            .find_clear(self.icursor)
            .ok_or_else(|| Error::fatal("no free inodes left"))?;
        self.inodes.set(i);
        self.icursor = i + 1;
        if dir {
            self.new_dirs[i / self.geo.ipg as usize] += 1;
        }
        Ok(i as u32 + 1)
    }

    /// The next free run of at most `max` blocks.
    fn alloc_extent(&mut self, max: u32) -> Result<(u32, u32)> {
        let i = self
            .blocks
            .find_clear(self.cursor)
            .ok_or_else(|| Error::fatal("no free blocks left"))?;
        // stay within the group
        let fdb = self.geo.fdb as usize;
        let bpg = self.geo.bpg as usize;
        let group_end = (i / bpg + 1) * bpg;
        let n = self.blocks.clear_run(i, (max as usize).min(group_end - i));
        self.blocks.set_range(i, n);
        self.cursor = i + n;
        Ok(((i + fdb) as u32, n as u32))
    }

    /// Lay out content of `size` bytes with the given non-zero blocks.
    pub fn plan(
        &mut self,
        size: u64,
        present: Option<&[(u64, u64)]>,
        want_map: bool,
    ) -> Result<DataLayout> {
        let bs = u64::from(self.geo.bs);
        let ppb = bs / 4;
        let nblocks = size.div_ceil(bs);
        if nblocks > max_file_blocks(self.geo.bs) {
            return Err(Error::fatal("file too large for the block size"));
        }
        let all = [(0u64, nblocks)];
        let ranges = present.unwrap_or(&all);
        let mut lay = DataLayout {
            i_block: [0; N_BLOCKS],
            nblocks: 0,
            ops: Vec::new(),
            ptrs: Vec::new(),
            map: want_map.then(|| vec![0u32; nblocks as usize]),
        };
        // current pointer blocks: (key, index in ptrs)
        let mut ind: Option<(u64, usize)> = None;
        let mut dind: Option<(u64, usize)> = None;
        let mut top_dind: Option<usize> = None;
        let mut tind: Option<usize> = None;
        for &(start, len) in ranges {
            let mut l = start;
            let end = start + len;
            while l < end {
                let seg_end;
                if l < NDIR_BLOCKS as u64 {
                    seg_end = end.min(NDIR_BLOCKS as u64);
                } else {
                    let k = (l - NDIR_BLOCKS as u64) / ppb;
                    seg_end = end.min(NDIR_BLOCKS as u64 + (k + 1) * ppb);
                    if ind.map(|(key, _)| key) != Some(k) {
                        let idx = if k == 0 {
                            let (p, idx) = self.new_ptr(&mut lay)?;
                            lay.i_block[IND_BLOCK] = p;
                            idx
                        } else {
                            // the double indirect block holding this one
                            let (parent, slot) = if k <= ppb {
                                let d = match top_dind {
                                    Some(d) => d,
                                    None => {
                                        let (p, d) = self.new_ptr(&mut lay)?;
                                        lay.i_block[DIND_BLOCK] = p;
                                        top_dind = Some(d);
                                        d
                                    }
                                };
                                (d, (k - 1) as usize)
                            } else {
                                let t = k - 1 - ppb;
                                let dkey = t / ppb;
                                let d = match dind {
                                    Some((key, d)) if key == dkey => d,
                                    _ => {
                                        let ti = match tind {
                                            Some(ti) => ti,
                                            None => {
                                                let (p, ti) = self.new_ptr(&mut lay)?;
                                                lay.i_block[TIND_BLOCK] = p;
                                                tind = Some(ti);
                                                ti
                                            }
                                        };
                                        let (p, d) = self.new_ptr(&mut lay)?;
                                        lay.ptrs[ti][dkey as usize] = p;
                                        dind = Some((dkey, d));
                                        d
                                    }
                                };
                                (d, (t % ppb) as usize)
                            };
                            let (p, idx) = self.new_ptr(&mut lay)?;
                            lay.ptrs[parent][slot] = p;
                            idx
                        };
                        ind = Some((k, idx));
                    }
                }
                while l < seg_end {
                    let (p, n) = self.alloc_extent((seg_end - l) as u32)?;
                    lay.ops.push(Op::Data {
                        logical: l,
                        phys: p,
                        len: n,
                    });
                    for j in 0..u64::from(n) {
                        let lb = l + j;
                        let pb = p + j as u32;
                        if lb < NDIR_BLOCKS as u64 {
                            lay.i_block[lb as usize] = pb;
                        } else {
                            let (_, idx) = ind.unwrap();
                            lay.ptrs[idx][((lb - NDIR_BLOCKS as u64) % ppb) as usize] = pb;
                        }
                        if let Some(m) = &mut lay.map {
                            m[lb as usize] = pb;
                        }
                    }
                    lay.nblocks += u64::from(n);
                    l += u64::from(n);
                }
            }
        }
        Ok(lay)
    }

    fn new_ptr(&mut self, lay: &mut DataLayout) -> Result<(u32, usize)> {
        let (p, _) = self.alloc_extent(1)?;
        let idx = lay.ptrs.len();
        lay.ptrs.push(vec![0u32; (self.geo.bs / 4) as usize]);
        lay.ops.push(Op::Ptr { phys: p, idx });
        lay.nblocks += 1;
        Ok((p, idx))
    }

    /// Write content laid out by [`Fs::plan`], read from `src`.
    pub fn write_data(
        &mut self,
        lay: &DataLayout,
        size: u64,
        src: &mut dyn ChunkSource,
    ) -> Result<()> {
        let bs = self.geo.bs as usize;
        let chunk = (src.chunk_size(bs).max(bs) / bs) * bs;
        let mut buf = std::mem::take(&mut self.buf);
        if buf.len() < chunk {
            // zeroed memory is mapped lazily: only the bytes used count
            buf = vec![0; chunk];
        }
        let mut pos = 0u64; // bytes consumed from the source
        let mut pblock = vec![0u8; bs];
        let result = (|| -> Result<()> {
            for op in &lay.ops {
                match *op {
                    Op::Ptr { phys, idx } => {
                        for (i, p) in lay.ptrs[idx].iter().enumerate() {
                            wr32(&mut pblock, i * 4, *p);
                        }
                        self.dev.write_at(u64::from(phys) * bs as u64, &pblock)?;
                    }
                    Op::Data { logical, phys, len } => {
                        let start = logical * bs as u64;
                        // skip holes
                        while pos < start {
                            let n = ((start - pos) as usize).min(chunk);
                            let got = fill_from(src, &mut buf[..n])?;
                            pos += n as u64;
                            if got < n {
                                pos = start;
                            }
                        }
                        let span = len as usize * bs;
                        let bytes = (size - start).min(span as u64) as usize;
                        let off = u64::from(phys) * bs as u64;
                        if let Some(mem) = self.dev.as_mut_slice() {
                            let dst = &mut mem[off as usize..off as usize + span];
                            let got = fill_from(src, &mut dst[..bytes])?;
                            dst[got..].fill(0);
                        } else {
                            let mut done = 0usize;
                            while done < span {
                                let n = (span - done).min(chunk);
                                let at = off + done as u64;
                                // read straight into the device's buffers
                                if let Some(dst) = self.dev.write_space(at, n)? {
                                    let want = bytes.saturating_sub(done).min(dst.len());
                                    let got = fill_from(src, &mut dst[..want])?;
                                    dst[got..].fill(0);
                                    done += dst.len();
                                    continue;
                                }
                                let want = bytes.saturating_sub(done).min(n);
                                let got = fill_from(src, &mut buf[..want])?;
                                buf[got..n].fill(0);
                                self.dev.write_at(at, &buf[..n])?;
                                done += n;
                            }
                        }
                        pos = start + bytes as u64;
                    }
                }
            }
            Ok(())
        })();
        self.buf = buf;
        result
    }

    /// Lay out and write in-memory content; returns the layout.
    pub fn write_bytes(&mut self, data: &[u8], want_map: bool) -> Result<DataLayout> {
        let lay = self.plan(data.len() as u64, None, want_map)?;
        self.write_data(&lay, data.len() as u64, &mut BytesSource { data, pos: 0 })?;
        Ok(lay)
    }

    pub fn read_block(&mut self, b: u32, buf: &mut [u8]) -> Result<()> {
        self.dev
            .read_at(u64::from(b) * u64::from(self.geo.bs), buf)
            .ctx(|| format!("reading block {b}"))
    }

    fn inode_offset(&self, n: u32) -> Result<u64> {
        if n == 0 || n > self.geo.inodes() {
            return Err(Error::fatal(format!("inode {n} out of range")));
        }
        let g = (n - 1) / self.geo.ipg;
        let idx = (n - 1) % self.geo.ipg;
        Ok(
            u64::from(self.gds[g as usize].inode_table()) * u64::from(self.geo.bs)
                + u64::from(idx) * INODE_SIZE as u64,
        )
    }

    /// An inode, as modified so far.
    pub fn inode(&mut self, n: u32) -> Result<Inode> {
        if let Some(i) = self.table.get(&n) {
            return Ok(*i);
        }
        let off = self.inode_offset(n)?;
        let mut raw = [0u8; INODE_SIZE];
        self.dev
            .read_at(off, &mut raw)
            .ctx(|| format!("reading inode {n}"))?;
        Ok(Inode(raw))
    }

    /// Blocks of an existing inode: physical block of each logical block
    /// (0 for holes) and the pointer blocks.
    pub fn inode_blocks(&mut self, ino: &Inode) -> Result<(Vec<u32>, Vec<u32>)> {
        let bs = u64::from(self.geo.bs);
        let t = ino.file_type();
        if !(t == S_IFREG || t == S_IFDIR || (t == S_IFLNK && ino.blocks() != 0)) {
            return Ok((Vec::new(), Vec::new()));
        }
        let n = ino.size64().div_ceil(bs) as usize;
        let mut data = vec![0u32; n];
        let mut meta = Vec::new();
        for (i, d) in data.iter_mut().enumerate().take(NDIR_BLOCKS) {
            *d = ino.block(i);
        }
        let ppb = (bs / 4) as usize;
        let mut buf = vec![0u8; bs as usize];
        // (block, depth, first logical block)
        let mut stack: Vec<(u32, u32, usize)> = Vec::new();
        let starts = [
            NDIR_BLOCKS,
            NDIR_BLOCKS + ppb,
            NDIR_BLOCKS + ppb + ppb * ppb,
        ];
        for (k, depth) in [(IND_BLOCK, 1u32), (DIND_BLOCK, 2), (TIND_BLOCK, 3)] {
            let b = ino.block(k);
            if b != 0 && starts[depth as usize - 1] < n {
                stack.push((b, depth, starts[depth as usize - 1]));
            }
        }
        while let Some((b, depth, first)) = stack.pop() {
            if b >= self.geo.blocks {
                return Err(Error::fatal(format!("block {b} out of range")));
            }
            meta.push(b);
            self.read_block(b, &mut buf)?;
            let span = ppb.pow(depth - 1);
            for i in 0..ppb {
                let p = rd32(&buf, i * 4);
                let l = first + i * span;
                if p == 0 || l >= n {
                    continue;
                }
                if depth == 1 {
                    data[l] = p;
                } else {
                    stack.push((p, depth - 1, l));
                }
            }
        }
        Ok((data, meta))
    }

    /// Entries of an existing directory: (name, inode, file type byte).
    pub fn read_dir(&mut self, ino: &Inode) -> Result<Vec<(Vec<u8>, u32, u8)>> {
        let (data, _) = self.inode_blocks(ino)?;
        let bs = self.geo.bs as usize;
        let mut buf = vec![0u8; bs];
        let mut out = Vec::new();
        for b in data {
            if b == 0 {
                continue;
            }
            self.read_block(b, &mut buf)?;
            let mut off = 0;
            while off + DIRENT_HEADER <= bs {
                let child = rd32(&buf, off);
                let rec = usize::from(rd16(&buf, off + 4));
                let nlen = if self.filetype {
                    usize::from(buf[off + 6])
                } else {
                    usize::from(rd16(&buf, off + 6)) & 0xff
                };
                if rec < DIRENT_HEADER || off + rec > bs || DIRENT_HEADER + nlen > rec {
                    return Err(Error::fatal("starting image: corrupt directory"));
                }
                if child != 0 {
                    let name = buf[off + DIRENT_HEADER..off + DIRENT_HEADER + nlen].to_vec();
                    let ft = if self.filetype { buf[off + 7] } else { 0 };
                    out.push((name, child, ft));
                }
                off += rec;
            }
        }
        Ok(out)
    }

    /// Fill the free blocks with `value`.
    fn fill_free(&mut self, value: u8) -> Result<()> {
        let bs = self.geo.bs as usize;
        let fill = vec![value; (1 << 20).max(bs)];
        let per = fill.len() / bs;
        let mut i = 0usize;
        while let Some(start) = self.blocks.find_clear(i) {
            let n = self.blocks.clear_run(start, usize::MAX);
            let mut done = 0;
            while done < n {
                let k = (n - done).min(per);
                let b = (start + done) as u64 + u64::from(self.geo.fdb);
                self.dev.write_at(b * bs as u64, &fill[..k * bs])?;
                done += k;
            }
            i = start + n;
        }
        Ok(())
    }

    /// Write the inodes, bitmaps, descriptors and superblocks.
    pub fn finish(&mut self, fill: u8) -> Result<()> {
        for b in std::mem::take(&mut self.deferred_free) {
            if b >= self.geo.fdb && b < self.geo.blocks {
                self.blocks.clear((b - self.geo.fdb) as usize);
            }
        }
        if fill != 0 {
            self.fill_free(fill)?;
        }
        let geo = self.geo.clone();
        let bs = geo.bs as usize;

        // inode tables
        let mut numbers: Vec<u32> = self.table.keys().copied().collect();
        numbers.sort_unstable();
        let per_block = (bs / INODE_SIZE) as u32;
        let mut block = vec![0u8; bs];
        let mut i = 0;
        while i < numbers.len() {
            let n = numbers[i];
            let g = (n - 1) / geo.ipg;
            let idx = (n - 1) % geo.ipg;
            let first = g * geo.ipg + (idx / per_block) * per_block + 1;
            let blk = self.gds[g as usize].inode_table() + idx / per_block;
            if self.existing {
                self.read_block(blk, &mut block)?;
            } else {
                block.fill(0);
            }
            while i < numbers.len() && numbers[i] < first + per_block {
                let k = (numbers[i] - first) as usize;
                block[k * INODE_SIZE..(k + 1) * INODE_SIZE]
                    .copy_from_slice(self.table[&numbers[i]].as_bytes());
                i += 1;
            }
            self.dev
                .write_at(blk as u64 * bs as u64, &block)
                .ctx(|| "writing inodes".to_string())?;
        }

        // bitmaps and counters
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        for g in 0..geo.groups {
            let start = (geo.group_start(g) - geo.fdb) as usize;
            let len = geo.group_len(g) as usize;
            self.blocks.copy_bytes(start, len, &mut block);
            for k in len..bs * 8 {
                block[k / 8] |= 1 << (k % 8);
            }
            let used = self.blocks.count_set(start, len);
            let gd = &mut self.gds[g as usize];
            gd.set_free_blocks_count((len - used) as u16);
            free_blocks += (len - used) as u64;
            let bb = gd.block_bitmap();
            self.dev.write_at(u64::from(bb) * bs as u64, &block)?;

            let istart = g as usize * geo.ipg as usize;
            let ilen = geo.ipg as usize;
            self.inodes.copy_bytes(istart, ilen, &mut block);
            for k in ilen..bs * 8 {
                block[k / 8] |= 1 << (k % 8);
            }
            let iused = self.inodes.count_set(istart, ilen);
            let gd = &mut self.gds[g as usize];
            gd.set_free_inodes_count((ilen - iused) as u16);
            free_inodes += (ilen - iused) as u64;
            let dirs = u32::from(gd.used_dirs_count()) + self.new_dirs[g as usize];
            gd.set_used_dirs_count(dirs as u16);
            let ib = gd.inode_bitmap();
            self.dev.write_at(u64::from(ib) * bs as u64, &block)?;
        }
        self.new_dirs.iter_mut().for_each(|d| *d = 0);
        self.sb.set_free_blocks_count(free_blocks as u32);
        self.sb.set_free_inodes_count(free_inodes as u32);

        // descriptors and superblocks
        let mut gdt = vec![0u8; geo.gdt_blocks as usize * bs];
        for (g, gd) in self.gds.iter().enumerate() {
            gdt[g * GROUP_DESC_SIZE..(g + 1) * GROUP_DESC_SIZE].copy_from_slice(gd.as_bytes());
        }
        for g in 0..geo.groups {
            if !geo.has_super(g) {
                continue;
            }
            let mut sb = self.sb.clone();
            sb.set_block_group_nr(g as u16);
            let start = u64::from(geo.group_start(g)) * bs as u64;
            let sb_off = if g == 0 { SUPERBLOCK_OFFSET } else { start };
            self.dev.write_at(sb_off, sb.as_bytes())?;
            let gdt_off = (u64::from(geo.group_start(g)) + 1) * bs as u64;
            let gdt_off = if g == 0 {
                u64::from(geo.fdb + 1) * bs as u64
            } else {
                gdt_off
            };
            self.dev.write_at(gdt_off, &gdt)?;
        }
        self.dev.flush().ctx(|| "writing the image".to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(size: u64, bs: u32, present: Option<&[(u64, u64)]>) -> u64 {
        let mut dev = crate::device::MemDevice::new();
        let geo = Geometry::new(bs, 300_000, 100).unwrap();
        let mut fs = Fs::create(
            &mut dev,
            NewImage {
                geo,
                reserved: 0,
                time: 0,
                creator_os: 0,
                label: None,
            },
        )
        .unwrap();
        fs.plan(size, present, false).unwrap().nblocks
    }

    #[test]
    fn block_counts() {
        let bs = 1024u32;
        let ppb = 256u64;
        for blocks in [
            0,
            1,
            12,
            13,
            12 + ppb,
            12 + ppb + 1,
            12 + ppb + ppb * 2 + 5,
            12 + ppb + ppb * ppb,
            12 + ppb + ppb * ppb + 1,
            12 + ppb + ppb * ppb + ppb * 3 + 7,
        ] {
            let size = blocks * u64::from(bs);
            assert_eq!(
                count_blocks(size, bs, None),
                brute(size, bs, None),
                "{blocks}"
            );
        }
        let sets: [&[(u64, u64)]; 4] = [
            &[(0, 1), (5000, 3)],
            &[(20, 1), (30, 1), (300, 600)],
            &[(70_000, 10), (70_050, 300)],
            &[(3, 400), (1000, 1), (67_000, 5000)],
        ];
        for s in sets {
            let size = 75_000 * u64::from(bs);
            assert_eq!(
                count_blocks(size, bs, Some(s)),
                brute(size, bs, Some(s)),
                "{s:?}"
            );
        }
    }

    #[test]
    fn geometry() {
        let g = Geometry::new(1024, 8194, 2000).unwrap();
        assert_eq!(g.groups, 2);
        assert!(g.bpg < 8192);
        let g = Geometry::new(4096, 65536, 100).unwrap();
        assert_eq!((g.groups, g.bpg), (2, 32768));
        assert!(Geometry::new(1024, 3, 10).is_none());
    }

    #[test]
    fn bitmaps() {
        let mut b = Bitmap::new(200);
        b.set_range(0, 70);
        assert_eq!(b.find_clear(0), Some(70));
        assert_eq!(b.clear_run(70, 1000), 130);
        b.set(199);
        assert_eq!(b.count_set(0, 200), 71);
        assert_eq!(b.clear_run(70, 1000), 129);
    }

    #[test]
    fn bitmap_ranges_match_individual_bits() {
        let len = 137;
        for start in 0..=len {
            for count in 0..=len - start {
                let mut bitmap = Bitmap::new(len);
                let mut bits = vec![false; len];
                for i in (0..len).step_by(17) {
                    bitmap.set(i);
                    bits[i] = true;
                }
                bitmap.set_range(start, count);
                bits[start..start + count].fill(true);
                for (i, &bit) in bits.iter().enumerate() {
                    assert_eq!(bitmap.get(i), bit, "start {start}, count {count}, bit {i}");
                }
                for from in [0, start, start + count, len, len + 1] {
                    for max in [0, 1, 63, 64, usize::MAX] {
                        let expected = bits
                            .iter()
                            .skip(from)
                            .take(max)
                            .take_while(|&&bit| !bit)
                            .count();
                        assert_eq!(bitmap.clear_run(from, max), expected);
                    }
                }
                for byte_start in (0..=len).step_by(8) {
                    let n = len - byte_start;
                    let mut actual = vec![0xff; n.div_ceil(8) + 2];
                    let mut expected = vec![0; actual.len()];
                    for (i, &bit) in bits[byte_start..].iter().enumerate() {
                        if bit {
                            expected[i / 8] |= 1 << (i % 8);
                        }
                    }
                    bitmap.copy_bytes(byte_start, n, &mut actual);
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
