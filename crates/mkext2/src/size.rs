//! Exact image sizing.
//!
//! The content's needs (blocks, including pointer and directory blocks, and
//! inodes) are counted from the tree; then the smallest image is searched
//! for whose geometry holds the content plus its metadata, keeps the
//! requested free blocks besides the reserve, and has at least one free
//! inode per `bytes_per_inode` bytes of free unreserved space.

use crate::error::{Error, Result};
use crate::fs::{Geometry, count_blocks};
use crate::ondisk::*;
use crate::tree::{Body, ROOT, Tree, dir_blocks};

/// Blocks of `lost+found`: 16, whatever the block size (as xgenext2fs).
pub(crate) fn lost_found_blocks(_bs: u32) -> u64 {
    16
}

/// What the content needs.
#[derive(Clone, Debug)]
pub(crate) struct Content {
    /// Blocks of everything but the root directory.
    pub blocks: u64,
    /// Blocks of the root directory without and with `lost+found`.
    pub root_blocks: [u64; 2],
    /// Inodes, counting the reserved ones (1 to 10) but not `lost+found`.
    pub inodes: u64,
    /// `lost+found` is created when blocks are reserved.
    pub lost_found: bool,
    /// Blocks of `lost+found`, with its pointer block.
    pub lost_found_blocks: u64,
}

/// Blocks a node needs (data and pointers).
pub(crate) fn node_blocks(tree: &Tree, id: usize, bs: u32) -> u64 {
    let n = &tree.nodes[id];
    match &n.body {
        Body::Dir(d) => {
            let nb = dir_blocks(d.entries.iter().map(|(name, _)| &name[..]), bs as usize);
            count_blocks(nb * u64::from(bs), bs, None)
        }
        Body::File(f) => count_blocks(f.size, bs, f.present.as_deref()),
        Body::Symlink(t) if t.len() >= FAST_SYMLINK_MAX => 1,
        _ => 0,
    }
}

impl Content {
    pub fn of(tree: &Tree, bs: u32) -> Content {
        let mut blocks = 0u64;
        for id in 1..tree.nodes.len() {
            blocks += node_blocks(tree, id, bs);
        }
        let root = tree.dir(ROOT);
        let names = || root.entries.iter().map(|(n, _)| &n[..]);
        let lost_found = root.get(b"lost+found").is_none();
        let nb = |extra: Option<&'static [u8]>| {
            let n = dir_blocks(extra.into_iter().chain(names()), bs as usize);
            count_blocks(n * u64::from(bs), bs, None)
        };
        Content {
            blocks,
            root_blocks: [nb(None), nb(Some(b"lost+found"))],
            inodes: u64::from(EXT2_FIRST_INO - 1) + (tree.nodes.len() as u64 - 1),
            lost_found,
            // (16 blocks need a pointer block with 1 KiB blocks)
            lost_found_blocks: count_blocks(lost_found_blocks(bs) * u64::from(bs), bs, None),
        }
    }
}

/// Sizing settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub bs: u32,
    /// Reserved percentage.
    pub reserve_pct: f64,
    pub bytes_per_inode: f64,
    pub min_inodes: u64,
}

/// A chosen size.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub geo: Geometry,
    pub reserved: u32,
    pub lost_found: bool,
}

pub(crate) fn reserved_for(blocks: u64, pct: f64) -> u64 {
    (blocks as f64 * pct / 100.0).floor() as u64
}

/// The layout of a `blocks`-block image, or how many blocks are missing
/// at least.
fn try_size(blocks: u64, extra: u64, c: &Content, p: &Params) -> std::result::Result<Layout, u64> {
    let bs = u64::from(p.bs);
    let reserved = reserved_for(blocks, p.reserve_pct);
    let lf = c.lost_found && reserved > 0;
    let content =
        c.blocks + c.root_blocks[usize::from(lf)] + if lf { c.lost_found_blocks } else { 0 };
    let used = c.inodes + u64::from(lf);
    let mut want = used.max(p.min_inodes);
    let mut best: Option<Geometry> = None;
    for _ in 0..64 {
        let Some(geo) = Geometry::new(p.bs, blocks, want) else {
            break;
        };
        let avail = geo.data_blocks();
        if avail < content {
            return Err(content - avail + reserved + extra);
        }
        let unreserved = (avail - content).saturating_sub(reserved);
        let free_inodes = ((unreserved * bs) as f64 / p.bytes_per_inode).ceil() as u64;
        let enough = u64::from(geo.inodes()) >= used + free_inodes;
        best = Some(geo);
        if enough {
            break;
        }
        want = used + free_inodes;
    }
    let Some(geo) = best else {
        // too many inodes for the groups (more groups are needed), or a
        // last group too small for its metadata (try the next size)
        let per_group = u64::from(p.bs) * 8;
        let fdb = u64::from(p.bs == 1024);
        let groups = blocks.saturating_sub(fdb).div_ceil(per_group).max(1);
        let groups_needed = want.div_ceil(per_group);
        if groups_needed > groups {
            let min = fdb + (groups_needed - 1) * per_group + 1;
            return Err(min.saturating_sub(blocks).max(1));
        }
        return Err(1);
    };
    let free = geo.data_blocks() - content;
    if free < reserved + extra {
        return Err(reserved + extra - free);
    }
    Ok(Layout {
        geo,
        reserved: reserved as u32,
        lost_found: lf,
    })
}

/// The smallest layout of at least `start` blocks with `extra` free blocks
/// besides the reserve.
fn smallest(start: u64, extra: u64, c: &Content, p: &Params) -> Result<Layout> {
    let mut n = start.max(1);
    loop {
        if n > u64::from(u32::MAX) {
            return Err(Error::fatal(
                "the content does not fit in an ext2 image with this block size",
            ));
        }
        match try_size(n, extra, c, p) {
            Ok(l) => return Ok(l),
            Err(d) => n += d.max(1),
        }
    }
}

/// The size for the content without `-b`: the smallest that fits, then
/// readjusted.
pub(crate) fn compute(
    c: &Content,
    p: &Params,
    readjust: impl Fn(u64) -> Result<crate::options::Readjusted>,
) -> Result<Layout> {
    let base = c.blocks + c.root_blocks[0];
    let needed = smallest(base, 0, c, p)?;
    let blocks = u64::from(needed.geo.blocks);
    match readjust(blocks)? {
        crate::options::Readjusted::Extra(0) => Ok(needed),
        crate::options::Readjusted::Extra(k) => smallest(blocks, k, c, p),
        crate::options::Readjusted::Size(v) if v <= blocks => Ok(needed),
        crate::options::Readjusted::Size(v) => smallest(v, 0, c, p),
    }
}

/// The layout for a requested size, raised when the content does not fit;
/// the flag says whether it was raised.
pub(crate) fn fixed(blocks: u64, c: &Content, p: &Params) -> Result<(Layout, bool)> {
    match try_size(blocks, 0, c, p) {
        Ok(l) => Ok((l, false)),
        Err(_) => Ok((smallest(blocks, 0, c, p)?, true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(blocks: u64, inodes: u64) -> Content {
        Content {
            blocks,
            root_blocks: [1, 1],
            inodes,
            lost_found: true,
            lost_found_blocks: 12,
        }
    }

    #[test]
    fn smallest_is_minimal() {
        let p = Params {
            bs: 1024,
            reserve_pct: 5.0,
            bytes_per_inode: 16384.0,
            min_inodes: 0,
        };
        for (blocks, inodes) in [(0, 10), (100, 50), (9000, 300), (50_000, 4000)] {
            let c = content(blocks, inodes);
            let l = smallest(1, 0, &c, &p).unwrap();
            let n = u64::from(l.geo.blocks);
            assert!(try_size(n, 0, &c, &p).is_ok());
            // nothing slightly smaller works
            for m in n.saturating_sub(50)..n {
                assert!(try_size(m, 0, &c, &p).is_err(), "{m} < {n}");
            }
        }
    }
}
