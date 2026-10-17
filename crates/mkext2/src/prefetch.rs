//! Parallel read-ahead of regular files for directory layers.
//!
//! The size estimation pass walks the source trees in exactly the order the
//! build consumes them, so it records the regular files to read. Worker
//! threads then read those files ahead of time, in chunks, while the main
//! thread allocates blocks and writes the image. The chunks go into one
//! ring buffer of fixed size: they are reserved and consumed in file
//! order, so read-ahead memory is bounded and allocated once, however many
//! or large the files are. The content is identical either way; only the
//! timing changes.

use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::error::{Error, Result};
use crate::fs::{ChunkSource, read_full, show};

/// Largest piece of a file read at once (a multiple of every block size).
const CHUNK: u64 = 4 << 20;
/// Memory used by read-ahead data at most: the size of the ring.
const RING: u64 = 64 << 20;

/// A piece of a file to read.
struct Piece {
    file: usize,
    offset: u64,
    len: usize,
}

/// The ring bytes reserved for one piece.
struct Slot {
    piece: usize,
    start: usize,
    /// The worker is done with it.
    filled: bool,
}

/// Ring space, reserved in piece order and released in the same order (the
/// consumer reads the pieces in order), so it is a plain circular queue.
struct Ring {
    cap: usize,
    slots: VecDeque<Slot>,
    /// Where the next reservation starts.
    head: usize,
    /// The newest slots start again at the beginning of the ring.
    wrapped: bool,
}

impl Ring {
    /// Reserve `len` bytes (at most `cap`); returns the start.
    fn reserve(&mut self, piece: usize, len: usize) -> Option<usize> {
        let start = match self.slots.front() {
            None => 0,
            Some(front) => {
                let tail = front.start;
                if !self.wrapped && self.head + len <= self.cap {
                    self.head
                } else if !self.wrapped && len <= tail {
                    self.wrapped = true;
                    0
                } else if self.wrapped && self.head + len <= tail {
                    self.head
                } else {
                    return None;
                }
            }
        };
        self.head = start + len;
        self.slots.push_back(Slot {
            piece,
            start,
            filled: false,
        });
        Some(start)
    }

    /// Release the oldest slots that are filled and before `released`.
    fn release(&mut self, released: usize) -> bool {
        let mut any = false;
        while let Some(front) = self.slots.front() {
            if front.piece >= released || !front.filled {
                break;
            }
            let old = front.start;
            self.slots.pop_front();
            any = true;
            match self.slots.front() {
                None => {
                    self.head = 0;
                    self.wrapped = false;
                }
                Some(f) if f.start < old => self.wrapped = false,
                Some(_) => {}
            }
        }
        any
    }

    fn fill(&mut self, piece: usize) {
        let i = self.slots.partition_point(|s| s.piece < piece);
        self.slots[i].filled = true;
    }
}

struct State {
    /// Next piece to read.
    next: usize,
    /// The consumer is past every piece before this one...
    consumed: usize,
    /// ...and done with the data of every piece before this one.
    released: usize,
    ring: Ring,
    /// Read pieces: (start in the ring, bytes read).
    results: HashMap<usize, io::Result<(usize, usize)>>,
    stop: bool,
}

/// The ring's bytes. Each slot is written by one worker while reserved and
/// not filled, then only read by the consumer until released; the state
/// mutex orders these accesses.
struct Buffer(Box<[UnsafeCell<u8>]>);

unsafe impl Sync for Buffer {}

impl Buffer {
    fn new(len: usize) -> Self {
        // zeroed memory is mapped lazily: only the bytes used are touched
        let raw = Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut [UnsafeCell<u8>];
        // UnsafeCell<u8> has the layout of u8
        Buffer(unsafe { Box::from_raw(raw) })
    }

    fn ptr(&self, start: usize) -> *mut u8 {
        UnsafeCell::raw_get(self.0[start..].as_ptr())
    }
}

struct Shared {
    files: Vec<Arc<Path>>,
    pieces: Vec<Piece>,
    buf: Buffer,
    state: Mutex<State>,
    /// A piece was read (the consumer waits for these).
    ready: Condvar,
    /// Ring space was released (the workers wait for these).
    room: Condvar,
}

/// Background readers for a list of files.
pub(crate) struct Prefetcher {
    shared: Arc<Shared>,
    /// First piece of each file.
    first: Vec<usize>,
    workers: Vec<JoinHandle<()>>,
}

impl Prefetcher {
    /// Start reading `items` (path, expected size, not 0) in order.
    pub fn start(items: Vec<(Arc<Path>, u64)>, threads: usize) -> Option<Self> {
        Self::start_with(items, threads, CHUNK, RING)
    }

    /// [`Prefetcher::start`] with pieces of `chunk` bytes in a ring of
    /// `ring` bytes (at least `chunk`).
    fn start_with(
        items: Vec<(Arc<Path>, u64)>,
        threads: usize,
        chunk: u64,
        ring: u64,
    ) -> Option<Self> {
        if items.is_empty() || threads == 0 {
            return None;
        }
        let mut first = Vec::with_capacity(items.len());
        let mut pieces = Vec::with_capacity(items.len());
        let mut files = Vec::with_capacity(items.len());
        for (file, (p, size)) in items.into_iter().enumerate() {
            first.push(pieces.len());
            let mut offset = 0;
            while offset < size {
                let len = (size - offset).min(chunk);
                pieces.push(Piece {
                    file,
                    offset,
                    len: len as usize,
                });
                offset += len;
            }
            files.push(p);
        }
        // no larger than all the pieces together; never smaller than one
        let total = pieces.iter().map(|p| p.len as u64).sum::<u64>();
        let cap = total.min(ring) as usize;
        let shared = Arc::new(Shared {
            files,
            pieces,
            buf: Buffer::new(cap),
            state: Mutex::new(State {
                next: 0,
                consumed: 0,
                released: 0,
                ring: Ring {
                    cap,
                    slots: VecDeque::new(),
                    head: 0,
                    wrapped: false,
                },
                results: HashMap::new(),
                stop: false,
            }),
            ready: Condvar::new(),
            room: Condvar::new(),
        });
        let workers = (0..threads)
            .map(|_| {
                let sh = shared.clone();
                std::thread::spawn(move || worker(&sh))
            })
            .collect();
        Some(Prefetcher {
            shared,
            first,
            workers,
        })
    }

    /// The first piece of file `file`, if it was not passed.
    pub fn take(&self, file: usize) -> Option<io::Result<Head>> {
        self.take_piece(*self.first.get(file)?)
    }

    /// Piece `idx`, if not passed. Every piece before it is passed.
    fn take_piece(&self, idx: usize) -> Option<io::Result<Head>> {
        let mut st = self.shared.state.lock().unwrap();
        if idx < st.consumed {
            return None;
        }
        // drop anything scheduled before this piece: it was skipped
        if st.consumed < idx {
            st.results.retain(|&k, _| k >= idx);
            st.consumed = idx;
            st.released = idx;
            if st.ring.release(idx) {
                self.shared.room.notify_all();
            }
        }
        loop {
            if let Some(r) = st.results.remove(&idx) {
                st.consumed = idx + 1;
                return Some(r.map(|(start, len)| Head {
                    shared: self.shared.clone(),
                    idx,
                    start,
                    len,
                }));
            }
            if st.next <= idx && st.stop {
                return None;
            }
            st = self.shared.ready.wait(st).unwrap();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.stop = true;
        }
        self.shared.room.notify_all();
        self.shared.ready.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

/// A piece read ahead, in the ring until dropped.
pub(crate) struct Head {
    shared: Arc<Shared>,
    idx: usize,
    start: usize,
    len: usize,
}

impl Head {
    fn bytes(&self) -> &[u8] {
        // filled before it was handed out and not reused until dropped
        unsafe { std::slice::from_raw_parts(self.shared.buf.ptr(self.start), self.len) }
    }

    fn piece(&self) -> &Piece {
        &self.shared.pieces[self.idx]
    }

    /// The piece read in full, so the file may go on.
    fn complete(&self) -> bool {
        self.len == self.piece().len
    }

    /// The next piece of the same file.
    fn next_piece(&self) -> Option<usize> {
        let next = self.idx + 1;
        let p = self.shared.pieces.get(next)?;
        (p.file == self.piece().file).then_some(next)
    }
}

impl Drop for Head {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().unwrap();
        st.released = st.released.max(self.idx + 1);
        let released = st.released;
        if st.ring.release(released) {
            self.shared.room.notify_all();
        }
    }
}

fn read_piece(sh: &Shared, piece: &Piece, dst: &mut [u8]) -> io::Result<usize> {
    let mut f = File::open(&sh.files[piece.file])?;
    if piece.offset > 0 {
        f.seek(SeekFrom::Start(piece.offset))?;
    }
    read_full(&mut f, dst)
}

fn worker(sh: &Shared) {
    loop {
        let (idx, start) = {
            let mut st = sh.state.lock().unwrap();
            loop {
                if st.stop || st.next >= sh.pieces.len() {
                    return;
                }
                let i = st.next;
                if i < st.consumed {
                    // passed already
                    st.next += 1;
                    continue;
                }
                // wait for room in the ring; the consumer releases it
                if let Some(start) = st.ring.reserve(i, sh.pieces[i].len) {
                    st.next += 1;
                    break (i, start);
                }
                st = sh.room.wait(st).unwrap();
            }
        };
        let piece = &sh.pieces[idx];
        // reserved for this worker alone until marked filled
        let dst = unsafe { std::slice::from_raw_parts_mut(sh.buf.ptr(start), piece.len) };
        let res = read_piece(sh, piece, dst);
        let mut st = sh.state.lock().unwrap();
        st.ring.fill(idx);
        if idx >= st.consumed {
            st.results.insert(idx, res.map(|n| (start, n)));
            sh.ready.notify_all();
        } else {
            let released = st.released;
            if st.ring.release(released) {
                sh.room.notify_all();
            }
        }
    }
}

/// File content read ahead piece by piece, then (if a piece came back
/// short, or a piece was not read ahead) from the file itself, with `fread`
/// semantics up to `limit` bytes.
pub(crate) struct PrefetchedSource<'a> {
    pub prefetcher: &'a Prefetcher,
    pub head: Option<Head>,
    /// Bytes of the current piece consumed.
    pub pos: usize,
    /// Bytes of the file consumed.
    pub done: u64,
    pub path: &'a Path,
    pub limit: u64,
    pub rest: Option<File>,
    pub name: &'a [u8],
}

impl PrefetchedSource<'_> {
    fn err(&self, e: io::Error) -> Error {
        Error::io(format!("reading {}", show(self.name)), e)
    }
}

impl ChunkSource for PrefetchedSource<'_> {
    fn chunk_size(&self, _block_size: usize) -> usize {
        CHUNK as usize
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.done >= self.limit {
            return Ok(0);
        }
        let want = ((self.limit - self.done) as usize).min(buf.len());
        while let Some(head) = &self.head {
            let bytes = head.bytes();
            if self.pos < bytes.len() {
                let n = (bytes.len() - self.pos).min(want);
                buf[..n].copy_from_slice(&bytes[self.pos..self.pos + n]);
                self.pos += n;
                self.done += n as u64;
                return Ok(n);
            }
            // the piece is used up: go on with the next one
            let next = head.complete().then(|| head.next_piece()).flatten();
            self.head = None;
            self.pos = 0;
            if let Some(next) = next {
                self.head = self
                    .prefetcher
                    .take_piece(next)
                    .transpose()
                    .map_err(|e| self.err(e))?;
            }
        }
        // past the pieces read ahead: continue from the file
        if self.rest.is_none() {
            let mut f = File::open(self.path).map_err(|e| self.err(e))?;
            f.seek(SeekFrom::Start(self.done))
                .map_err(|e| self.err(e))?;
            self.rest = Some(f);
        }
        let f = self.rest.as_mut().unwrap();
        let n = read_full(f as &mut dyn Read, &mut buf[..want]).map_err(|e| self.err(e))?;
        self.done += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(cap: usize) -> Ring {
        Ring {
            cap,
            slots: VecDeque::new(),
            head: 0,
            wrapped: false,
        }
    }

    #[test]
    fn ring_wraps_and_never_overlaps_live_slots() {
        let mut r = ring(10);
        assert_eq!(r.reserve(0, 4), Some(0));
        assert_eq!(r.reserve(1, 4), Some(4));
        // 2 bytes left at the end, the start is taken
        assert_eq!(r.reserve(2, 3), None);
        r.fill(0);
        r.fill(1);
        assert!(r.release(1));
        // slot 1 still holds 4..8: wrap to the start, before it
        assert_eq!(r.reserve(2, 3), Some(0));
        assert_eq!(r.reserve(3, 2), None);
        // releasing needs both filled and passed: slot 2 is not filled
        assert!(r.release(3));
        assert_eq!(r.reserve(3, 5), Some(3));
        assert_eq!(r.reserve(4, 3), None);
        r.fill(2);
        assert!(r.release(3));
        // slot 3 holds 3..8
        assert_eq!(r.reserve(4, 4), None);
        assert_eq!(r.reserve(4, 3), Some(0));
        r.fill(3);
        r.fill(4);
        assert!(!r.release(3));
        assert!(r.release(5));
        // empty again: the whole ring is free
        assert_eq!(r.reserve(5, 10), Some(0));
    }

    fn files(dir: &Path, sizes: &[usize]) -> Vec<(Arc<Path>, Vec<u8>)> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let p = dir.join(format!("f{i}"));
                let data: Vec<u8> = (0..n).map(|k| (k * 7 + i) as u8).collect();
                std::fs::write(&p, &data).unwrap();
                (Arc::from(p), data)
            })
            .collect()
    }

    fn read_all(pf: &Prefetcher, file: usize, path: &Path, size: u64) -> Option<Vec<u8>> {
        let head = pf.take(file)?.unwrap();
        let mut src = PrefetchedSource {
            prefetcher: pf,
            head: Some(head),
            pos: 0,
            done: 0,
            path,
            limit: size,
            rest: None,
            name: b"f",
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 5];
        loop {
            let n = src.read_chunk(&mut buf).unwrap();
            if n == 0 {
                return Some(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn files_larger_than_the_ring_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let fs = files(dir.path(), &[1, 40, 17, 16, 3, 64, 9]);
        let items = fs
            .iter()
            .map(|(p, d)| (p.clone(), d.len() as u64))
            .collect();
        let pf = Prefetcher::start_with(items, 3, 8, 20).unwrap();
        for (i, (p, d)) in fs.iter().enumerate() {
            assert_eq!(read_all(&pf, i, p, d.len() as u64).as_ref(), Some(d));
        }
    }

    #[test]
    fn skipped_files_and_partly_read_files_release_their_space() {
        let dir = tempfile::tempdir().unwrap();
        let fs = files(dir.path(), &[30, 12, 30, 5, 30, 7]);
        let items = fs
            .iter()
            .map(|(p, d)| (p.clone(), d.len() as u64))
            .collect();
        let pf = Prefetcher::start_with(items, 2, 8, 16).unwrap();
        // the first piece of file 0 only
        drop(pf.take(0).unwrap().unwrap());
        // files 1 and 2 are skipped
        let (p, d) = &fs[3];
        assert_eq!(read_all(&pf, 3, p, 5).as_ref(), Some(d));
        // passed files are not read ahead any more
        assert!(pf.take(1).is_none());
        let (p, d) = &fs[5];
        assert_eq!(read_all(&pf, 5, p, 7).as_ref(), Some(d));
    }

    #[test]
    fn a_file_that_shrank_is_read_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let fs = files(dir.path(), &[20, 4]);
        let items = fs
            .iter()
            .map(|(p, d)| (p.clone(), d.len() as u64))
            .collect();
        std::fs::write(&fs[0].0, &fs[0].1[..10]).unwrap();
        let pf = Prefetcher::start_with(items, 2, 8, 16).unwrap();
        assert_eq!(read_all(&pf, 0, &fs[0].0, 20), Some(fs[0].1[..10].to_vec()));
        assert_eq!(read_all(&pf, 1, &fs[1].0, 4).as_ref(), Some(&fs[1].1));
    }
}
