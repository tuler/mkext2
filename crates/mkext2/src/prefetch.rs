//! Parallel read-ahead of regular files for directory layers.
//!
//! The size estimation pass walks the source trees in exactly the order the
//! build consumes them, so it records the regular files to read. Worker
//! threads then read the beginning of each file ahead of time, bounded in
//! memory, while the main thread allocates blocks and writes the image.
//! The content is identical either way; only the timing changes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::error::{Error, Result};
use crate::fs::{ChunkSource, read_full, show};

/// Largest prefix of a file read ahead (a multiple of every block size).
const HEAD: u64 = 8 << 20;
/// Memory used by read-ahead data at most.
const INFLIGHT: usize = 256 << 20;

struct State {
    next: usize,
    consumed: usize,
    inflight: usize,
    results: HashMap<usize, io::Result<Vec<u8>>>,
    stop: bool,
}

struct Shared {
    items: Vec<(PathBuf, u64)>,
    state: Mutex<State>,
    cond: Condvar,
}

/// Background readers for a list of files.
pub(crate) struct Prefetcher {
    shared: Arc<Shared>,
    index: HashMap<PathBuf, usize>,
    workers: Vec<JoinHandle<()>>,
}

impl Prefetcher {
    /// Start reading `items` (path, expected size) in order.
    pub fn start(items: Vec<(PathBuf, u64)>, threads: usize) -> Option<Self> {
        if items.is_empty() || threads == 0 {
            return None;
        }
        let mut index = HashMap::with_capacity(items.len());
        for (i, (p, _)) in items.iter().enumerate() {
            index.entry(p.clone()).or_insert(i);
        }
        let shared = Arc::new(Shared {
            items,
            state: Mutex::new(State {
                next: 0,
                consumed: 0,
                inflight: 0,
                results: HashMap::new(),
                stop: false,
            }),
            cond: Condvar::new(),
        });
        let workers = (0..threads)
            .map(|_| {
                let sh = shared.clone();
                std::thread::spawn(move || worker(&sh))
            })
            .collect();
        Some(Prefetcher {
            shared,
            index,
            workers,
        })
    }

    pub fn has(&self, path: &Path) -> bool {
        self.index.contains_key(path)
    }

    /// The read-ahead prefix of `path`, if it was scheduled and not passed.
    pub fn take(&self, path: &Path) -> Option<io::Result<Vec<u8>>> {
        let &idx = self.index.get(path)?;
        let mut st = self.shared.state.lock().unwrap();
        if idx < st.consumed {
            return None;
        }
        // drop anything scheduled before this file: it was skipped
        if st.consumed < idx {
            let stale: Vec<usize> = st.results.keys().copied().filter(|&k| k < idx).collect();
            for k in stale {
                if let Some(Ok(v)) = st.results.remove(&k) {
                    st.inflight -= v.len();
                }
            }
            st.consumed = idx;
            self.shared.cond.notify_all();
        }
        loop {
            if let Some(r) = st.results.remove(&idx) {
                if let Ok(v) = &r {
                    st.inflight -= v.len();
                }
                st.consumed = idx + 1;
                self.shared.cond.notify_all();
                return Some(r);
            }
            if st.next <= idx && st.stop {
                return None;
            }
            st = self.shared.cond.wait(st).unwrap();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        {
            let mut st = self.shared.state.lock().unwrap();
            st.stop = true;
        }
        self.shared.cond.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(sh: &Shared) {
    loop {
        let idx = {
            let mut st = sh.state.lock().unwrap();
            loop {
                if st.stop || st.next >= sh.items.len() {
                    return;
                }
                // stay within the memory budget, but never block the file
                // the consumer is waiting for
                if st.inflight < INFLIGHT || st.next <= st.consumed {
                    break;
                }
                st = sh.cond.wait(st).unwrap();
            }
            let i = st.next;
            st.next += 1;
            if i < st.consumed {
                continue;
            }
            i
        };
        let (path, size) = &sh.items[idx];
        let want = (*size).min(HEAD) as usize;
        let res = File::open(path).and_then(|mut f| {
            let mut v = vec![0u8; want];
            let n = read_full(&mut f, &mut v)?;
            v.truncate(n);
            Ok(v)
        });
        let mut st = sh.state.lock().unwrap();
        if idx >= st.consumed {
            if let Ok(v) = &res {
                st.inflight += v.len();
            }
            st.results.insert(idx, res);
        }
        sh.cond.notify_all();
    }
}

/// File content made of a read-ahead prefix followed by the rest of the
/// file, read with `fread` semantics up to `limit` bytes.
pub(crate) struct PrefetchedSource<'a> {
    pub head: Vec<u8>,
    pub pos: usize,
    pub path: &'a Path,
    pub limit: u64,
    pub rest: Option<File>,
    pub name: &'a [u8],
}

impl ChunkSource for PrefetchedSource<'_> {
    fn chunk_size(&self, _block_size: usize) -> usize {
        4 << 20
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let head_len = (self.head.len() as u64).min(self.limit) as usize;
        if self.pos < head_len {
            let n = (head_len - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        // past the prefix: continue from the file if more is expected
        let done = self.pos as u64;
        if done >= self.limit {
            return Ok(0);
        }
        if self.rest.is_none() {
            let mut f = File::open(self.path)
                .map_err(|e| Error::io(format!("reading {}", show(self.name)), e))?;
            f.seek(SeekFrom::Start(done))
                .map_err(|e| Error::io(format!("reading {}", show(self.name)), e))?;
            self.rest = Some(f);
        }
        let want = ((self.limit - done) as usize).min(buf.len());
        let f = self.rest.as_mut().unwrap();
        let n = read_full(f as &mut dyn Read, &mut buf[..want])
            .map_err(|e| Error::io(format!("reading {}", show(self.name)), e))?;
        self.pos += n;
        Ok(n)
    }
}
