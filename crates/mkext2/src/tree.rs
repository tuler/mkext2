//! The filesystem content as it is assembled from the layers, before
//! anything is written: nodes (inodes to be) and directory entries.

use std::path::PathBuf;

use crate::error::Result;
use crate::fs::Fs;
use crate::hash::FxHashMap;
use crate::ondisk::*;

pub(crate) type NodeId = usize;

pub(crate) const ROOT: NodeId = 0;

/// Where a regular file's data comes from.
pub(crate) enum DataSrc {
    Empty,
    Path(PathBuf),
    /// Member `member` (counting from 0) of the archive of layer `layer`.
    Tar {
        layer: usize,
        member: u64,
    },
}

pub(crate) struct FileData {
    pub size: u64,
    pub src: DataSrc,
    /// Non-zero blocks, with `-z`.
    pub present: Option<Vec<(u64, u64)>>,
}

pub(crate) struct Dir {
    pub parent: NodeId,
    /// Entries in order, without `.` and `..`.
    pub entries: Vec<(Box<[u8]>, NodeId)>,
    index: Option<FxHashMap<Box<[u8]>, NodeId>>,
    /// Existing directories are read from the starting image on first use.
    pub loaded: bool,
    /// Entries were added to an existing directory.
    pub changed: bool,
}

/// Directories with more entries than this get a hash index.
const INDEX_AFTER: usize = 12;

impl Dir {
    pub fn new(parent: NodeId) -> Self {
        Dir {
            parent,
            entries: Vec::new(),
            index: None,
            loaded: true,
            changed: false,
        }
    }

    pub fn get(&self, name: &[u8]) -> Option<NodeId> {
        match &self.index {
            Some(ix) => ix.get(name).copied(),
            None => self
                .entries
                .iter()
                .find(|(n, _)| &n[..] == name)
                .map(|&(_, id)| id),
        }
    }

    fn push(&mut self, name: &[u8], id: NodeId) {
        let name: Box<[u8]> = name.into();
        if let Some(ix) = &mut self.index {
            ix.insert(name.clone(), id);
        } else if self.entries.len() >= INDEX_AFTER {
            let mut ix: FxHashMap<Box<[u8]>, NodeId> = self.entries.iter().cloned().collect();
            ix.insert(name.clone(), id);
            self.index = Some(ix);
        }
        self.entries.push((name, id));
    }
}

pub(crate) enum Body {
    Dir(Box<Dir>),
    File(Box<FileData>),
    Symlink(Vec<u8>),
    Device(u32, u32),
    /// FIFOs, sockets, and existing nodes whose content does not change.
    Other,
}

pub(crate) struct Node {
    /// File type (`S_IFMT` bits).
    pub kind: u32,
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub links: u32,
    pub body: Body,
    /// Inode number (assigned when writing; existing nodes have theirs).
    pub ino: u32,
    /// The node comes from the starting image.
    pub existing: bool,
    /// An existing node whose inode must be rewritten.
    pub dirty: bool,
}

/// Metadata of a new node.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Meta {
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
}

pub(crate) struct Tree {
    pub nodes: Vec<Node>,
    /// Existing inode number to node.
    existing: FxHashMap<u32, NodeId>,
}

impl Tree {
    /// A new tree whose root has the given metadata.
    pub fn new(root: Meta) -> Self {
        let mut t = Tree {
            nodes: Vec::new(),
            existing: FxHashMap::default(),
        };
        t.nodes.push(Node {
            kind: S_IFDIR,
            perm: root.perm,
            uid: root.uid,
            gid: root.gid,
            atime: root.atime,
            ctime: root.ctime,
            mtime: root.mtime,
            links: 2,
            body: Body::Dir(Box::new(Dir::new(ROOT))),
            ino: EXT2_ROOT_INO,
            existing: false,
            dirty: false,
        });
        t
    }

    /// The tree of an existing image (directories are read when used).
    pub fn open(fs: &mut Fs<'_>) -> Result<Self> {
        let mut t = Tree {
            nodes: Vec::new(),
            existing: FxHashMap::default(),
        };
        let root = fs.inode(EXT2_ROOT_INO)?;
        if root.file_type() != S_IFDIR {
            return Err(crate::error::Error::fatal(
                "starting image: the root is not a directory",
            ));
        }
        t.add_existing(EXT2_ROOT_INO, &root, ROOT);
        Ok(t)
    }

    fn add_existing(&mut self, ino: u32, inode: &Inode, parent: NodeId) -> NodeId {
        let id = self.nodes.len();
        let kind = inode.file_type();
        let body = if kind == S_IFDIR {
            let mut d = Dir::new(if ino == EXT2_ROOT_INO { ROOT } else { parent });
            d.loaded = false;
            Body::Dir(Box::new(d))
        } else {
            Body::Other
        };
        self.nodes.push(Node {
            kind,
            perm: u32::from(inode.mode()) & S_IMASK,
            uid: inode.uid32(),
            gid: inode.gid32(),
            atime: inode.atime(),
            ctime: inode.ctime(),
            mtime: inode.mtime(),
            links: u32::from(inode.links_count()),
            body,
            ino,
            existing: true,
            dirty: false,
        });
        self.existing.insert(ino, id);
        id
    }

    pub fn dir(&self, id: NodeId) -> &Dir {
        match &self.nodes[id].body {
            Body::Dir(d) => d,
            _ => unreachable!("not a directory"),
        }
    }

    pub fn dir_mut(&mut self, id: NodeId) -> &mut Dir {
        match &mut self.nodes[id].body {
            Body::Dir(d) => d,
            _ => unreachable!("not a directory"),
        }
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.nodes[id].kind == S_IFDIR
    }

    /// Read an existing directory's entries from the image.
    pub fn load(&mut self, id: NodeId, fs: Option<&mut Fs<'_>>) -> Result<()> {
        if self.dir(id).loaded {
            return Ok(());
        }
        let fs = fs.expect("existing directories need the image");
        let inode = fs.inode(self.nodes[id].ino)?;
        let entries = fs.read_dir(&inode)?;
        self.dir_mut(id).loaded = true;
        for (name, child, _) in entries {
            if name == b"." || name == b".." {
                continue;
            }
            let cid = match self.existing.get(&child) {
                Some(&c) => c,
                None => {
                    let ci = fs.inode(child)?;
                    self.add_existing(child, &ci, id)
                }
            };
            self.dir_mut(id).push(&name, cid);
        }
        Ok(())
    }

    /// Look a name up in a directory (`.` and `..` included).
    pub fn lookup(
        &mut self,
        dir: NodeId,
        name: &[u8],
        fs: Option<&mut Fs<'_>>,
    ) -> Result<Option<NodeId>> {
        if name == b"." {
            return Ok(Some(dir));
        }
        if name == b".." {
            return Ok(Some(self.dir(dir).parent));
        }
        self.load(dir, fs)?;
        Ok(self.dir(dir).get(name))
    }

    /// Resolve the components of a path from `start`, through directories.
    pub fn resolve(
        &mut self,
        start: NodeId,
        comps: &[&[u8]],
        mut fs: Option<&mut Fs<'_>>,
    ) -> Result<Option<NodeId>> {
        let mut cur = start;
        for c in comps {
            if !self.is_dir(cur) {
                return Ok(None);
            }
            match self.lookup(cur, c, fs.as_deref_mut())? {
                Some(n) => cur = n,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// Resolve an image path from `start` (`/` restarts from the root).
    pub fn resolve_path(
        &mut self,
        start: NodeId,
        path: &[u8],
        fs: Option<&mut Fs<'_>>,
    ) -> Result<Option<NodeId>> {
        let start = if path.first() == Some(&b'/') {
            ROOT
        } else {
            start
        };
        let comps = components(path);
        self.resolve(start, &comps, fs)
    }

    /// Add an entry for an existing node (a hard link, or a new node).
    pub fn link(&mut self, dir: NodeId, name: &[u8], id: NodeId) {
        let d = self.dir_mut(dir);
        d.push(name, id);
        d.changed = true;
        self.nodes[id].links += 1;
        if self.nodes[id].existing {
            self.nodes[id].dirty = true;
        }
    }

    /// Create a node and its entry in `dir`.
    pub fn create(
        &mut self,
        dir: NodeId,
        name: &[u8],
        kind: u32,
        meta: Meta,
        body: Body,
    ) -> NodeId {
        let id = self.nodes.len();
        let is_dir = kind == S_IFDIR;
        let body = if is_dir {
            Body::Dir(Box::new(Dir::new(dir)))
        } else {
            body
        };
        self.nodes.push(Node {
            kind,
            perm: meta.perm & S_IMASK,
            uid: meta.uid,
            gid: meta.gid,
            atime: meta.atime,
            ctime: meta.ctime,
            mtime: meta.mtime,
            // "." of a new directory
            links: u32::from(is_dir),
            body,
            ino: 0,
            existing: false,
            dirty: false,
        });
        self.link(dir, name, id);
        if is_dir {
            // its ".."
            self.nodes[dir].links += 1;
            if self.nodes[dir].existing {
                self.nodes[dir].dirty = true;
            }
        }
        id
    }

    /// Create a directory as the first entry of `dir`.
    pub fn create_first(&mut self, dir: NodeId, name: &[u8], meta: Meta) -> NodeId {
        let id = self.create(dir, name, S_IFDIR, meta, Body::Other);
        let d = self.dir_mut(dir);
        let e = d.entries.pop().expect("just added");
        d.entries.insert(0, e);
        id
    }

    /// Change a node's permissions and owner, and its times if given.
    pub fn update(
        &mut self,
        id: NodeId,
        perm: u32,
        uid: u32,
        gid: u32,
        times: Option<(u32, u32, u32)>,
    ) {
        let n = &mut self.nodes[id];
        n.perm = perm & S_IMASK;
        n.uid = uid;
        n.gid = gid;
        if let Some((a, c, m)) = times {
            n.atime = a;
            n.ctime = c;
            n.mtime = m;
        }
        if n.existing {
            n.dirty = true;
        }
    }

    /// Load every existing directory (for listings).
    pub fn load_all(&mut self, mut fs: Option<&mut Fs<'_>>) -> Result<()> {
        let mut i = 0;
        while i < self.nodes.len() {
            if self.is_dir(i) && !self.dir(i).loaded {
                self.load(i, fs.as_deref_mut())?;
            }
            i += 1;
        }
        Ok(())
    }

    /// Depth-first list of (path, node), the root first.
    pub fn paths(&self) -> Vec<(Vec<u8>, NodeId)> {
        let mut out = Vec::new();
        let mut stack = vec![(b"/".to_vec(), ROOT)];
        while let Some((path, id)) = stack.pop() {
            out.push((path.clone(), id));
            if let Body::Dir(d) = &self.nodes[id].body {
                for (name, c) in d.entries.iter().rev() {
                    stack.push((join(&path, name), *c));
                }
            }
        }
        out
    }
}

/// Path components, without empty ones and `.`.
pub(crate) fn components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|&c| c == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .collect()
}

/// `dir/name` for messages.
pub(crate) fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = dir.to_vec();
    if p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// Number of blocks of a directory with entries named `names` (besides
/// `.` and `..`): entries are packed in order, none crossing a block.
pub(crate) fn dir_blocks<'a>(names: impl Iterator<Item = &'a [u8]>, bs: usize) -> u64 {
    let mut blocks = 1u64;
    let mut off = dirent_len(1) + dirent_len(2);
    for n in names {
        let len = dirent_len(n.len());
        if off + len > bs {
            blocks += 1;
            off = 0;
        }
        off += len;
    }
    blocks
}

/// The blocks of a directory: `.`, `..` and `entries` (name, inode, file
/// type), then `extra` empty blocks.
pub(crate) fn dir_content(
    entries: impl Iterator<Item = (Vec<u8>, u32, u8)>,
    bs: usize,
    filetype: bool,
    extra: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; bs];
    let mut block = 0usize;
    let mut off = 0usize;
    // offset of the previous entry in the current block
    let mut last: Option<usize> = None;
    let mut put = |out: &mut Vec<u8>, name: &[u8], ino: u32, ft: u8| {
        let len = dirent_len(name.len());
        if off + len > bs {
            if let Some(p) = last {
                let start = block * bs + p;
                wr16(out, start + 4, (bs - p) as u16);
            }
            block += 1;
            out.resize((block + 1) * bs, 0);
            off = 0;
        }
        let start = block * bs + off;
        put_dirent(out, start, ino, len, if filetype { ft } else { 0 }, name);
        if !filetype {
            // without the feature the name length is 16 bits
            out[start + 7] = 0;
        }
        last = Some(off);
        off += len;
    };
    for (name, ino, ft) in entries {
        put(&mut out, &name, ino, ft);
    }
    if let Some(p) = last {
        let start = block * bs + p;
        wr16(&mut out, start + 4, (bs - p) as u16);
    }
    for _ in 0..extra {
        let start = out.len();
        out.resize(start + bs, 0);
        wr16(&mut out, start + 4, bs as u16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_packing() {
        let names: Vec<Vec<u8>> = (0..500).map(|i| vec![b'a'; 1 + i % 200]).collect();
        for bs in [1024usize, 2048, 4096] {
            let n = dir_blocks(names.iter().map(|n| &n[..]), bs);
            let all = std::iter::once((b".".to_vec(), 2, FT_DIR))
                .chain(std::iter::once((b"..".to_vec(), 2, FT_DIR)))
                .chain(names.iter().map(|n| (n.clone(), 12, FT_REG_FILE)));
            let c = dir_content(all, bs, true, 0);
            assert_eq!(c.len() as u64, n * bs as u64);
            // every block is fully covered by its entries
            for b in c.chunks(bs) {
                let mut off = 0;
                while off < bs {
                    let rec = usize::from(rd16(b, off + 4));
                    assert!(rec >= 12);
                    off += rec;
                }
                assert_eq!(off, bs);
            }
        }
    }
}
