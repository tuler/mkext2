//! Storage backends the image is written to.

use std::fs::File;
use std::io;
use std::sync::{Arc, mpsc};

/// Random-access storage for a filesystem image.
pub trait Device {
    /// Read `buf.len()` bytes at `offset`. Bytes past the end read as zeros.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    /// Write `buf` at `offset`.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;
    /// Resize the device (new space reads as zeros).
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Current size in bytes.
    fn size(&mut self) -> io::Result<u64>;
    /// Flush buffered state to stable storage (no-op for memory).
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// Direct mutable access to the backing bytes, when the device is memory.
    fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        None
    }
}

/// An image backed by a file, written with positioned I/O.
///
/// Writes are combined: the image is filled in several sequential streams
/// at once (one per block group in use), so each stream is accumulated
/// into a run and written in large pieces by a background thread.
pub struct FileDevice {
    file: Arc<File>,
    runs: Vec<Run>,
    pending: usize,
    writer: Option<Writer>,
}

struct Run {
    offset: u64,
    data: Vec<u8>,
}

struct Writer {
    tx: Option<mpsc::SyncSender<Run>>,
    handle: Option<std::thread::JoinHandle<io::Result<()>>>,
}

/// Size at which a run is written out.
const RUN_LIMIT: usize = 8 << 20;
/// Total buffered bytes before the largest run is written out.
const PENDING_LIMIT: usize = 64 << 20;
/// Bound searches over small, disjoint runs as well as their total bytes.
const RUN_COUNT_LIMIT: usize = 64;
/// Runs queued for the writer thread.
const QUEUE: usize = 4;

impl FileDevice {
    pub fn new(file: File) -> Self {
        FileDevice {
            file: Arc::new(file),
            runs: Vec::new(),
            pending: 0,
            writer: None,
        }
    }

    /// Write everything out and return the file.
    pub fn into_inner(mut self) -> io::Result<File> {
        self.sync_writes()?;
        self.file.try_clone()
    }

    fn submit(&mut self, run: Run) -> io::Result<()> {
        if self.writer.is_none() {
            let (tx, rx) = mpsc::sync_channel::<Run>(QUEUE);
            let file = self.file.clone();
            let handle = std::thread::spawn(move || -> io::Result<()> {
                for run in rx {
                    pwrite_full(&file, run.offset, &run.data)?;
                }
                Ok(())
            });
            self.writer = Some(Writer {
                tx: Some(tx),
                handle: Some(handle),
            });
        }
        let w = self.writer.as_mut().unwrap();
        if w.tx.as_ref().unwrap().send(run).is_err() {
            // the writer stopped on an error: report it
            return self.join_writer();
        }
        Ok(())
    }

    fn join_writer(&mut self) -> io::Result<()> {
        if let Some(mut w) = self.writer.take() {
            drop(w.tx.take());
            if let Some(h) = w.handle.take() {
                return h
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("writer thread panicked")));
            }
        }
        Ok(())
    }

    fn flush_run(&mut self, i: usize) -> io::Result<()> {
        let run = self.runs.swap_remove(i);
        self.pending -= run.data.len();
        self.submit(run)
    }

    /// Write all pending runs and wait until they are on the file.
    fn sync_writes(&mut self) -> io::Result<()> {
        while !self.runs.is_empty() {
            self.flush_run(self.runs.len() - 1)?;
        }
        self.join_writer()
    }
}

impl Drop for FileDevice {
    fn drop(&mut self) {
        let _ = self.sync_writes();
    }
}

#[cfg(unix)]
fn pread_full(f: &File, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !buf.is_empty() {
        match f.read_at(buf, off) {
            Ok(0) => {
                buf.fill(0);
                return Ok(());
            }
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn pwrite_full(f: &File, off: u64, buf: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
fn pread_full(f: &File, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off) {
            Ok(0) => {
                buf.fill(0);
                return Ok(());
            }
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn pwrite_full(f: &File, mut off: u64, mut buf: &[u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, off) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write")),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl Device for FileDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.sync_writes()?;
        pread_full(&self.file, offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow"))?;
        // Flush every older overlapping run before merging or queuing this
        // write. An append can extend across another pending run too.
        while let Some(i) = self
            .runs
            .iter()
            .position(|r| r.offset < end && offset < r.offset + r.data.len() as u64)
        {
            self.flush_run(i)?;
        }
        // extend a run ending exactly here
        if let Some(i) = self
            .runs
            .iter()
            .position(|r| r.offset + r.data.len() as u64 == offset)
        {
            let run = &mut self.runs[i];
            run.data.extend_from_slice(buf);
            self.pending += buf.len();
            if run.data.len() >= RUN_LIMIT {
                self.flush_run(i)?;
            }
        } else {
            if buf.len() >= RUN_LIMIT {
                return self.submit(Run {
                    offset,
                    data: buf.to_vec(),
                });
            }
            let mut data = Vec::with_capacity(RUN_LIMIT.min(buf.len() * 4).max(buf.len()));
            data.extend_from_slice(buf);
            self.pending += buf.len();
            self.runs.push(Run { offset, data });
        }
        while self.pending > PENDING_LIMIT || self.runs.len() > RUN_COUNT_LIMIT {
            let largest = (0..self.runs.len())
                .max_by_key(|&i| self.runs[i].data.len())
                .unwrap();
            self.flush_run(largest)?;
        }
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.sync_writes()?;
        self.file.set_len(len)
    }

    fn size(&mut self) -> io::Result<u64> {
        self.sync_writes()?;
        Ok(self.file.metadata()?.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sync_writes()
    }
}

/// An image held entirely in memory.
#[derive(Default)]
pub struct MemDevice {
    data: Vec<u8>,
}

impl MemDevice {
    pub fn new() -> Self {
        MemDevice { data: Vec::new() }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        MemDevice { data }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}

impl Device for MemDevice {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = self.data.len() as u64;
        if offset >= len {
            buf.fill(0);
            return Ok(());
        }
        let start = offset as usize;
        let avail = (self.data.len() - start).min(buf.len());
        buf[..avail].copy_from_slice(&self.data[start..start + avail]);
        buf[avail..].fill(0);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let end = offset as usize + buf.len();
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[offset as usize..end].copy_from_slice(buf);
        Ok(())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let len = usize::try_from(len).map_err(|_| {
            io::Error::new(io::ErrorKind::OutOfMemory, "image too large for memory")
        })?;
        if len > self.data.len() {
            // A fresh zeroed allocation lets the allocator hand out lazily
            // mapped zero pages instead of touching every byte.
            if self.data.is_empty() {
                self.data = vec![0u8; len];
            } else {
                self.data.resize(len, 0);
            }
        } else {
            self.data.truncate(len);
        }
        Ok(())
    }

    fn size(&mut self) -> io::Result<u64> {
        Ok(self.data.len() as u64)
    }

    fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.data)
    }
}

impl<D: Device + ?Sized> Device for Box<D> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(offset, buf)
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        (**self).set_len(len)
    }
    fn size(&mut self) -> io::Result<u64> {
        (**self).size()
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
    fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        (**self).as_mut_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_writes_preserve_call_order() {
        let mut file = FileDevice::new(tempfile::tempfile().unwrap());
        // Extending the second run also overwrites the first run.
        file.write_at(2, b"old").unwrap();
        file.write_at(0, b"a").unwrap();
        file.write_at(1, b"new!").unwrap();
        let mut actual = [0; 5];
        file.read_at(0, &mut actual).unwrap();
        assert_eq!(&actual, b"anew!");
    }

    #[test]
    fn size_includes_buffered_writes() {
        let mut file = FileDevice::new(tempfile::tempfile().unwrap());
        file.write_at(10, b"data").unwrap();
        assert_eq!(file.size().unwrap(), 14);
        file.set_len(2).unwrap();
        assert_eq!(file.size().unwrap(), 2);
    }

    #[test]
    fn interleaved_writes_match_memory() {
        let mut file = FileDevice::new(tempfile::tempfile().unwrap());
        let mut memory = MemDevice::new();
        // Enough disjoint runs to exercise eviction and the writer queue.
        for offset in (0..200).map(|i| i * 32) {
            file.write_at(offset, &[1; 8]).unwrap();
            memory.write_at(offset, &[1; 8]).unwrap();
        }
        let mut seed = 42u64;
        for _ in 0..1000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let offset = seed % 6400;
            let bytes = seed.to_le_bytes();
            file.write_at(offset, &bytes).unwrap();
            memory.write_at(offset, &bytes).unwrap();
        }
        let mut actual = vec![0; memory.as_slice().len()];
        file.read_at(0, &mut actual).unwrap();
        assert_eq!(actual, memory.as_slice());
    }
}
