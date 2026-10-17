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
    /// Room to write up to `len` bytes at `offset` in place, when the device
    /// buffers writes. The caller fills every byte of the slice returned (at
    /// least one, at most `len`), which is then written as by `write_at`.
    fn write_space(&mut self, offset: u64, len: usize) -> io::Result<Option<&mut [u8]>> {
        let _ = (offset, len);
        Ok(None)
    }
}

/// An image backed by a file, written with positioned I/O.
///
/// Writes are combined into runs of consecutive bytes and written in large
/// pieces by a background thread. Runs live in a fixed pool of buffers that
/// the writer hands back after each write, so the memory used stays within
/// [`POOL`] buffers of [`BUF`] bytes however large the image is, and is
/// allocated once instead of for every write.
pub struct FileDevice {
    file: Arc<File>,
    runs: Vec<Run>,
    /// Buffers ready for a new run.
    free: Vec<Vec<u8>>,
    /// Buffers allocated so far, at most [`POOL`].
    allocated: usize,
    writer: Option<Writer>,
}

struct Run {
    offset: u64,
    /// A buffer of [`BUF`] bytes, the first `used` of them pending.
    data: Vec<u8>,
    used: usize,
}

impl Run {
    fn end(&self) -> u64 {
        self.offset + self.used as u64
    }
}

struct Writer {
    tx: Option<mpsc::SyncSender<Run>>,
    /// Buffers coming back from the writer.
    back: mpsc::Receiver<Vec<u8>>,
    handle: Option<std::thread::JoinHandle<io::Result<()>>>,
}

/// Size of a buffer, the largest single write.
const BUF: usize = 8 << 20;
/// Buffers in the pool: the bytes waiting to be written stay within
/// `POOL * BUF`.
const POOL: usize = 4;

impl FileDevice {
    pub fn new(file: File) -> Self {
        FileDevice {
            file: Arc::new(file),
            runs: Vec::new(),
            free: Vec::new(),
            allocated: 0,
            writer: None,
        }
    }

    /// Write everything out and return the file.
    pub fn into_inner(mut self) -> io::Result<File> {
        self.sync_writes()?;
        self.file.try_clone()
    }

    fn writer(&mut self) -> &mut Writer {
        self.writer.get_or_insert_with(|| {
            // room for every buffer: sending never blocks
            let (tx, rx) = mpsc::sync_channel::<Run>(POOL);
            let (back_tx, back) = mpsc::sync_channel::<Vec<u8>>(POOL);
            let file = self.file.clone();
            let handle = std::thread::spawn(move || -> io::Result<()> {
                for run in rx {
                    pwrite_full(&file, run.offset, &run.data[..run.used])?;
                    let _ = back_tx.send(run.data);
                }
                Ok(())
            });
            Writer {
                tx: Some(tx),
                back,
                handle: Some(handle),
            }
        })
    }

    fn submit(&mut self, run: Run) -> io::Result<()> {
        let w = self.writer();
        if w.tx.as_ref().unwrap().send(run).is_err() {
            // the writer stopped on an error: report it
            return self.join_writer();
        }
        Ok(())
    }

    fn join_writer(&mut self) -> io::Result<()> {
        let Some(mut w) = self.writer.take() else {
            return Ok(());
        };
        drop(w.tx.take());
        let result = match w.handle.take() {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("writer thread panicked"))),
            None => Ok(()),
        };
        self.free.extend(w.back.try_iter());
        // buffers lost with a failed write are allocated again if needed
        self.allocated = self.free.len() + self.runs.len();
        result
    }

    fn flush_run(&mut self, i: usize) -> io::Result<()> {
        let run = self.runs.swap_remove(i);
        self.submit(run)
    }

    /// A buffer for a new run: a free one, a new one while the pool is not
    /// full, or one the writer hands back after writing out the largest
    /// pending run.
    fn take_buffer(&mut self) -> io::Result<Vec<u8>> {
        if let Some(w) = &self.writer {
            self.free.extend(w.back.try_iter());
        }
        if let Some(b) = self.free.pop() {
            return Ok(b);
        }
        if self.allocated < POOL {
            self.allocated += 1;
            // zeroed memory is mapped lazily, as it is first used
            return Ok(vec![0; BUF]);
        }
        if let Some(largest) = (0..self.runs.len()).max_by_key(|&i| self.runs[i].used) {
            self.flush_run(largest)?;
        }
        // every buffer is with the writer now: wait for one
        match self.writer().back.recv() {
            Ok(b) => Ok(b),
            // the writer stopped on an error: report it
            Err(_) => {
                self.join_writer()?;
                Err(io::Error::other("writer thread stopped"))
            }
        }
    }

    /// Room for up to `len` (> 0) bytes at `offset`, at the end of a run.
    fn space(&mut self, offset: u64, len: usize) -> io::Result<&mut [u8]> {
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write offset overflow"))?;
        // Write out full runs, and every older overlapping run before
        // queuing this write. An append can extend across another pending
        // run too.
        while let Some(i) = self
            .runs
            .iter()
            .position(|r| r.used == BUF || (r.offset < end && offset < r.end()))
        {
            self.flush_run(i)?;
        }
        // extend a run ending exactly here, or start one
        let i = match self.runs.iter().position(|r| r.end() == offset) {
            Some(i) => i,
            None => {
                let data = self.take_buffer()?;
                self.runs.push(Run {
                    offset,
                    data,
                    used: 0,
                });
                self.runs.len() - 1
            }
        };
        let run = &mut self.runs[i];
        let n = (BUF - run.used).min(len);
        run.used += n;
        Ok(&mut run.data[run.used - n..run.used])
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

    fn write_at(&mut self, mut offset: u64, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let dst = self.space(offset, buf.len())?;
            let n = dst.len();
            dst.copy_from_slice(&buf[..n]);
            offset += n as u64;
            buf = &buf[n..];
        }
        Ok(())
    }

    fn write_space(&mut self, offset: u64, len: usize) -> io::Result<Option<&mut [u8]>> {
        if len == 0 {
            return Ok(None);
        }
        self.space(offset, len).map(Some)
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
    fn write_space(&mut self, offset: u64, len: usize) -> io::Result<Option<&mut [u8]>> {
        (**self).write_space(offset, len)
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
    fn writes_larger_than_the_pool_match_memory() {
        let mut file = FileDevice::new(tempfile::tempfile().unwrap());
        let mut memory = MemDevice::new();
        let big: Vec<u8> = (0..BUF * POOL + BUF / 2 + 3)
            .map(|i| (i % 251) as u8)
            .collect();
        file.write_at(5, &big).unwrap();
        memory.write_at(5, &big).unwrap();
        // in place, across buffers, then over part of the big write
        let mut at = 7 + big.len() as u64;
        let mut left = BUF + 9;
        while left > 0 {
            let dst = file.write_space(at, left).unwrap().unwrap();
            dst.fill(0xa5);
            memory.write_at(at, &vec![0xa5; dst.len()]).unwrap();
            at += dst.len() as u64;
            left -= dst.len();
        }
        file.write_at(BUF as u64 - 2, b"overlap").unwrap();
        memory.write_at(BUF as u64 - 2, b"overlap").unwrap();
        let mut actual = vec![0; memory.as_slice().len()];
        file.read_at(0, &mut actual).unwrap();
        assert!(actual == memory.as_slice());
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
