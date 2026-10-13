//! A streaming tar reader following libarchive's interpretation of the
//! format (the library behind xgenext2fs' `-a`): ustar, GNU (long names,
//! old sparse), pax (including GNU sparse 0.x/1.0) and v7 archives, with
//! transparent gzip/bzip2/xz/zstd decompression.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use crate::error::{Error, Result};
use crate::ondisk::*;

const BLOCK: usize = 512;

/// One archive member, as libarchive's `archive_entry` would describe it.
#[derive(Clone, Debug, Default)]
pub struct TarEntry {
    pub path: Vec<u8>,
    /// Full mode including the file type bits (0 type for plain hard links).
    pub mode: u32,
    pub uid: i64,
    pub gid: i64,
    /// `archive_entry_size`: the logical size of the entry.
    pub size: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
    pub hardlink: Option<Vec<u8>>,
    pub symlink: Option<Vec<u8>>,
    pub devmajor: i64,
    pub devminor: i64,
}

impl TarEntry {
    pub fn file_type(&self) -> u32 {
        self.mode & S_IFMT
    }

    fn set_filetype(&mut self, t: u32) {
        self.mode = (self.mode & !S_IFMT) | t;
    }

    /// `archive_entry_copy_link`: replaces the symlink target when the
    /// entry is a symlink, otherwise sets a hard link.
    fn set_link(&mut self, target: Vec<u8>) {
        if self.symlink.is_some() {
            self.symlink = Some(target);
        } else {
            self.hardlink = Some(target);
        }
    }
}

/// Buffered input with peeking and fast skipping.
enum Input {
    File(BufReader<File>),
    Stream(BufReader<Box<dyn Read + Send>>),
}

impl Input {
    fn reader(&mut self) -> &mut dyn BufRead {
        match self {
            Input::File(r) => r,
            Input::Stream(r) => r,
        }
    }

    /// Read exactly `buf.len()` bytes; returns the number read (short at EOF).
    fn read_full(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let r = self.reader();
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

    /// Peek up to `n` bytes without consuming them.
    fn peek(&mut self, n: usize) -> io::Result<Vec<u8>> {
        // BufReader can only peek within its buffer; grow it via fill_buf
        // when the buffer holds less than needed.
        let r = self.reader();
        let buf = r.fill_buf()?;
        if buf.len() >= n || buf.is_empty() {
            return Ok(buf[..buf.len().min(n)].to_vec());
        }
        // Rare: buffer boundary in the middle of the peeked area.
        let mut out = buf.to_vec();
        let len = out.len();
        r.consume(len);
        let mut rest = vec![0u8; n - len];
        let got = {
            let mut k = 0;
            while k < rest.len() {
                match r.read(&mut rest[k..]) {
                    Ok(0) => break,
                    Ok(m) => k += m,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            k
        };
        out.extend_from_slice(&rest[..got]);
        // Put everything back by wrapping the reader.
        let replay = out.clone();
        let old = std::mem::replace(self, Input::Stream(BufReader::new(Box::new(io::empty()))));
        let inner: Box<dyn Read + Send> = match old {
            Input::File(f) => Box::new(io::Cursor::new(replay).chain(f)),
            Input::Stream(s) => Box::new(io::Cursor::new(replay).chain(s)),
        };
        *self = Input::Stream(BufReader::with_capacity(256 * 1024, inner));
        Ok(out)
    }

    fn skip(&mut self, mut n: u64) -> io::Result<u64> {
        let total = n;
        match self {
            Input::File(f) => {
                let buffered = f.buffer().len() as u64;
                if n <= buffered {
                    f.consume(n as usize);
                    return Ok(total);
                }
                let pos = f.stream_position()?;
                let end = f.get_mut().metadata()?.len();
                let target = (pos + n).min(end);
                f.seek(SeekFrom::Start(target))?;
                Ok(target - pos)
            }
            Input::Stream(s) => {
                while n > 0 {
                    let buf = s.fill_buf()?;
                    if buf.is_empty() {
                        break;
                    }
                    let k = buf.len().min(n as usize);
                    s.consume(k);
                    n -= k as u64;
                }
                Ok(total - n)
            }
        }
    }
}

/// Open a possibly compressed byte stream, peeling compression layers
/// like libarchive's `archive_read_support_filter_all`.
pub(crate) fn decompress(reader: Box<dyn Read + Send>) -> Result<(Box<dyn Read + Send>, bool)> {
    let mut r: Box<dyn BufRead + Send> = Box::new(BufReader::with_capacity(256 * 1024, reader));
    let mut compressed = false;
    for _ in 0..8 {
        let magic = r
            .fill_buf()
            .map_err(|e| Error::io("reading archive", e))?
            .to_vec();
        let next: Box<dyn Read + Send> = if magic.starts_with(&[0x1f, 0x8b]) {
            gzip(r)?
        } else if magic.starts_with(b"BZh") && magic.len() >= 4 && (b'1'..=b'9').contains(&magic[3])
        {
            bzip2(r)?
        } else if magic.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
            xz(r)?
        } else if magic.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            zstd(r)?
        } else {
            return Ok((Box::new(r), compressed));
        };
        compressed = true;
        r = Box::new(BufReader::with_capacity(256 * 1024, next));
    }
    Err(Error::fatal(
        "archive_read_next_header(): too many compression layers",
    ))
}

#[allow(dead_code)]
fn unsupported(name: &str) -> Error {
    Error::fatal(format!(
        "archive_read_next_header(): {name} compression support was not compiled in"
    ))
}

fn gzip(r: Box<dyn BufRead + Send>) -> Result<Box<dyn Read + Send>> {
    #[cfg(feature = "gzip")]
    {
        Ok(Box::new(flate2::bufread::MultiGzDecoder::new(r)))
    }
    #[cfg(not(feature = "gzip"))]
    {
        drop(r);
        Err(unsupported("gzip"))
    }
}

fn bzip2(r: Box<dyn BufRead + Send>) -> Result<Box<dyn Read + Send>> {
    #[cfg(feature = "bzip2")]
    {
        Ok(Box::new(::bzip2::bufread::MultiBzDecoder::new(r)))
    }
    #[cfg(not(feature = "bzip2"))]
    {
        drop(r);
        Err(unsupported("bzip2"))
    }
}

fn xz(r: Box<dyn BufRead + Send>) -> Result<Box<dyn Read + Send>> {
    #[cfg(feature = "xz")]
    {
        Ok(Box::new(liblzma::bufread::XzDecoder::new_multi_decoder(r)))
    }
    #[cfg(not(feature = "xz"))]
    {
        drop(r);
        Err(unsupported("xz"))
    }
}

fn zstd(r: Box<dyn BufRead + Send>) -> Result<Box<dyn Read + Send>> {
    #[cfg(feature = "zstd")]
    {
        let d = ::zstd::stream::read::Decoder::with_buffer(r).map_err(|e| Error::io("zstd", e))?;
        Ok(Box::new(d))
    }
    #[cfg(not(feature = "zstd"))]
    {
        drop(r);
        Err(unsupported("zstd"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Unknown,
    Tar,
    Ustar,
    Gnu,
    Pax,
}

/// Overrides collected from special headers, applied after the real header.
enum Special {
    LongName(Vec<u8>),
    LongLink(Vec<u8>),
    Pax(Vec<u8>),
}

/// Sparse map state gathered while parsing headers.
#[derive(Default)]
struct SparseInfo {
    entries: Vec<(i64, i64)>,
    gnu_major: Option<i64>,
    gnu_minor: Option<i64>,
    pending_offset: Option<i64>,
    realsize_override: Option<i64>,
    name_override: Option<Vec<u8>>,
}

/// Streaming reader of archive members.
pub struct TarReader {
    input: Input,
    format: Format,
    started: bool,
    finished: bool,
    // physical data of the current entry
    entry_bytes_remaining: i64,
    padding: i64,
    realsize: i64,
    segments: VecDeque<(i64, i64)>,
    // archive_read_data() emulation
    rd_offset: i64,
    rd_output: i64,
    rd_remaining: i64,
}

fn io_err(e: io::Error) -> Error {
    Error::io("archive_read_next_header()", e)
}

fn header_err(msg: &str) -> Error {
    Error::fatal(format!("archive_read_next_header(): {msg}"))
}

/// libarchive's `tar_atol`.
fn tar_atol(p: &[u8]) -> i64 {
    if p.first().is_some_and(|&c| c & 0x80 != 0) {
        tar_atol256(p)
    } else {
        tar_atol_base(p, 8)
    }
}

fn tar_atol_base(mut p: &[u8], base: i64) -> i64 {
    while let [b' ' | b'\t', rest @ ..] = p {
        p = rest;
    }
    let mut neg = false;
    if let [b'-', rest @ ..] = p {
        neg = true;
        p = rest;
    }
    let (maxval, limit, last) = if neg {
        (i64::MIN, -(i64::MIN / base), -(i64::MIN % base))
    } else {
        (i64::MAX, i64::MAX / base, i64::MAX % base)
    };
    let mut l: i64 = 0;
    for &c in p {
        let d = i64::from(c) - i64::from(b'0');
        if !(0..base).contains(&d) {
            break;
        }
        if l > limit || (l == limit && d >= last) {
            return maxval;
        }
        l = l * base + d;
    }
    if neg { -l } else { l }
}

fn tar_atol256(p: &[u8]) -> i64 {
    let mut c = p[0];
    let neg: u8;
    let mut l: u64;
    if c & 0x40 != 0 {
        neg = 0xff;
        c |= 0x80;
        l = !0;
    } else {
        neg = 0;
        c &= 0x7f;
        l = 0;
    }
    let mut cnt = p.len();
    let mut i = 0;
    while cnt > 8 {
        cnt -= 1;
        if c != neg {
            return if neg != 0 { i64::MIN } else { i64::MAX };
        }
        i += 1;
        c = p[i];
    }
    if (c ^ neg) & 0x80 != 0 {
        return if neg != 0 { i64::MIN } else { i64::MAX };
    }
    while cnt > 1 {
        cnt -= 1;
        l = (l << 8) | u64::from(c);
        i += 1;
        c = p[i];
    }
    l = (l << 8) | u64::from(c);
    l as i64
}

fn tar_atol10(p: &[u8]) -> i64 {
    tar_atol_base(p, 10)
}

/// pax time: seconds part of `[-]sec[.frac]`.
fn pax_time(p: &[u8]) -> i64 {
    let (neg, digits) = match p {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, p),
    };
    let mut s: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            break;
        }
        let d = i64::from(c - b'0');
        if s > i64::MAX / 10 || (s == i64::MAX / 10 && d > i64::MAX % 10) {
            s = i64::MAX;
            break;
        }
        s = s * 10 + d;
    }
    if neg { -s } else { s }
}

fn cstr(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    &field[..end]
}

fn is_null_block(h: &[u8]) -> bool {
    h.iter().all(|&c| c == 0)
}

/// libarchive's header checksum test (unsigned, then signed sums).
fn checksum_ok(h: &[u8]) -> bool {
    let field = &h[148..156];
    if field
        .iter()
        .any(|&c| c != b' ' && c != 0 && !(b'0'..=b'7').contains(&c))
    {
        return false;
    }
    let sum = tar_atol(field) as i32;
    let mut unsigned: i32 = 0;
    let mut signed: i32 = 0;
    for (i, &c) in h.iter().enumerate().take(BLOCK) {
        let (u, s) = if (148..156).contains(&i) {
            (32, 32)
        } else {
            (i32::from(c), i32::from(c as i8))
        };
        unsigned += u;
        signed += s;
    }
    sum == unsigned || sum == signed
}

fn valid_number_field(f: &[u8]) -> bool {
    // libarchive's validate_number_field
    if f[0] & 0x80 != 0 {
        return true;
    }
    let mut i = 0;
    while i < f.len() && (f[i] == b' ' || f[i] == 0) {
        i += 1;
    }
    let digits_start = i;
    while i < f.len() && (b'0'..=b'7').contains(&f[i]) {
        i += 1;
    }
    let _ = digits_start;
    while i < f.len() && (f[i] == b' ' || f[i] == 0) {
        i += 1;
    }
    i == f.len()
}

/// libarchive's `archive_read_format_tar_bid`.
fn tar_bid(h: &[u8]) -> i32 {
    if h.len() < BLOCK {
        return -1;
    }
    if h[0] == 0 && is_null_block(h) {
        return 10;
    }
    if !checksum_ok(h) {
        return 0;
    }
    let mut bid = 48;
    if &h[257..263] == b"ustar\0" && &h[263..265] == b"00" {
        bid += 56;
    }
    if &h[257..263] == b"ustar " && &h[263..265] == b" \0" {
        bid += 56;
    }
    let t = h[156];
    if t != 0 && !t.is_ascii_alphanumeric() {
        return 0;
    }
    bid += 2;
    let fields = [
        &h[100..108],
        &h[108..116],
        &h[116..124],
        &h[136..148],
        &h[124..136],
        &h[329..337],
        &h[337..345],
    ];
    if fields.iter().any(|f| !valid_number_field(f)) {
        bid = 0;
    }
    bid
}

impl TarReader {
    /// Read from a file, using seeks to skip member data when uncompressed.
    pub fn from_file(file: File) -> Result<Self> {
        let mut br = BufReader::with_capacity(256 * 1024, file);
        let magic = br.fill_buf().map_err(io_err)?.to_vec();
        let compressed = magic.starts_with(&[0x1f, 0x8b])
            || magic.starts_with(b"BZh")
            || magic.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00])
            || magic.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]);
        if compressed {
            let (r, _) = decompress(Box::new(br))?;
            Ok(Self::with_input(Input::Stream(BufReader::with_capacity(
                256 * 1024,
                r,
            ))))
        } else {
            Ok(Self::with_input(Input::File(br)))
        }
    }

    /// Read from any byte stream (possibly compressed).
    pub fn from_reader(reader: Box<dyn Read + Send>) -> Result<Self> {
        let (r, _) = decompress(reader)?;
        Ok(Self::with_input(Input::Stream(BufReader::with_capacity(
            256 * 1024,
            r,
        ))))
    }

    fn with_input(input: Input) -> Self {
        TarReader {
            input,
            format: Format::Unknown,
            started: false,
            finished: false,
            entry_bytes_remaining: 0,
            padding: 0,
            realsize: 0,
            segments: VecDeque::new(),
            rd_offset: 0,
            rd_output: 0,
            rd_remaining: 0,
        }
    }

    fn skip_current(&mut self) -> Result<()> {
        let n = self.rd_remaining.max(0) + self.entry_bytes_remaining.max(0) + self.padding.max(0);
        self.rd_remaining = 0;
        self.entry_bytes_remaining = 0;
        self.padding = 0;
        self.segments.clear();
        if n > 0 {
            let got = self.input.skip(n as u64).map_err(io_err)?;
            if got < n as u64 {
                return Err(header_err("Truncated tar archive"));
            }
        }
        Ok(())
    }

    fn read_block(&mut self, buf: &mut [u8; BLOCK]) -> Result<usize> {
        self.input.read_full(buf).map_err(io_err)
    }

    /// Read the body of a special entry (long name, pax header...).
    fn read_body(&mut self, h: &[u8]) -> Result<Vec<u8>> {
        let size = tar_atol(&h[124..136]);
        if !(0..=0x7fff_ffff).contains(&size) {
            return Err(header_err("Special header too large"));
        }
        let size = size as usize;
        let mut body = vec![0u8; size];
        let n = self.input.read_full(&mut body).map_err(io_err)?;
        if n < size {
            return Err(header_err("Truncated tar archive"));
        }
        let pad = (BLOCK - size % BLOCK) % BLOCK;
        if self.input.skip(pad as u64).map_err(io_err)? < pad as u64 {
            return Err(header_err("Truncated tar archive"));
        }
        Ok(body)
    }

    /// Advance to the next member (`archive_read_next_header`).
    pub fn next_entry(&mut self) -> Result<Option<TarEntry>> {
        if self.finished {
            return Ok(None);
        }
        self.skip_current()?;
        self.rd_offset = 0;
        self.rd_output = 0;
        self.rd_remaining = 0;

        let mut specials: Vec<Special> = Vec::new();
        let mut depth = 0;
        let mut h = [0u8; BLOCK];
        loop {
            depth += 1;
            if depth > 32 {
                return Err(header_err("Too many special headers"));
            }
            let n = self.read_block(&mut h)?;
            if n == 0 {
                if !self.started {
                    // an empty input is a valid empty archive
                    self.finished = true;
                    return Ok(None);
                }
                if specials.is_empty() {
                    self.finished = true;
                    return Ok(None);
                }
                return Err(header_err("Truncated tar archive"));
            }
            if n < BLOCK {
                if !self.started {
                    return Err(header_err("Unrecognized archive format"));
                }
                return Err(header_err("Truncated tar archive"));
            }
            if !self.started && tar_bid(&h) <= 0 {
                return Err(header_err("Unrecognized archive format"));
            }
            self.started = true;
            if h[0] == 0 && is_null_block(&h) {
                // end of archive; try to consume a second null block
                if let Ok(next) = self.input.peek(BLOCK) {
                    if next.len() == BLOCK && is_null_block(&next) {
                        let _ = self.input.skip(BLOCK as u64);
                    }
                }
                self.finished = true;
                return Ok(None);
            }
            if !checksum_ok(&h) {
                return Err(header_err("Damaged tar archive"));
            }
            match h[156] {
                b'A' => {
                    self.format = Format::Pax;
                    self.read_body(&h)?;
                }
                b'g' => {
                    self.format = Format::Pax;
                    self.read_body(&h)?;
                }
                b'K' => {
                    let body = self.read_body(&h)?;
                    specials.push(Special::LongLink(cstr(&body).to_vec()));
                }
                b'L' => {
                    let body = self.read_body(&h)?;
                    specials.push(Special::LongName(cstr(&body).to_vec()));
                }
                b'V' => {
                    self.read_body(&h)?;
                }
                b'x' | b'X' => {
                    self.format = Format::Pax;
                    let body = self.read_body(&h)?;
                    specials.push(Special::Pax(body));
                }
                _ => {
                    let mut sparse = SparseInfo::default();
                    let mut entry = self.parse_header(&h, &mut sparse)?;
                    // apply special headers, innermost (last read) first
                    for sp in specials.into_iter().rev() {
                        match sp {
                            Special::LongName(name) => entry.path = name,
                            Special::LongLink(link) => entry.set_link(link),
                            Special::Pax(text) => self.apply_pax(&mut entry, &text, &mut sparse)?,
                        }
                    }
                    self.finish_entry(&mut entry, sparse)?;
                    return Ok(Some(entry));
                }
            }
        }
    }

    /// Parse a real header block (ustar, GNU or v7).
    fn parse_header(&mut self, h: &[u8; BLOCK], sparse: &mut SparseInfo) -> Result<TarEntry> {
        let mut e = TarEntry::default();
        let magic = &h[257..265];
        let kind = if magic == b"ustar  \0" {
            self.format = Format::Gnu;
            Format::Gnu
        } else if &magic[..5] == b"ustar" {
            if self.format != Format::Pax {
                self.format = Format::Ustar;
            }
            Format::Ustar
        } else {
            self.format = Format::Tar;
            Format::Tar
        };

        // name (ustar prefix handled below)
        match kind {
            Format::Ustar => {
                let prefix = cstr(&h[345..500]);
                let name = cstr(&h[0..100]);
                if !prefix.is_empty() {
                    let mut p = prefix.to_vec();
                    if p.last() != Some(&b'/') {
                        p.push(b'/');
                    }
                    p.extend_from_slice(name);
                    e.path = p;
                } else {
                    e.path = name.to_vec();
                }
            }
            _ => e.path = cstr(&h[0..100]).to_vec(),
        }

        // header_common
        let linkname = cstr(&h[157..257]).to_vec();
        e.mode = tar_atol(&h[100..108]) as u32;
        e.uid = tar_atol(&h[108..116]);
        e.gid = tar_atol(&h[116..124]);
        self.entry_bytes_remaining = tar_atol(&h[124..136]);
        if self.entry_bytes_remaining < 0 {
            self.entry_bytes_remaining = 0;
            return Err(header_err("Tar entry has negative size"));
        }
        self.realsize = self.entry_bytes_remaining;
        e.size = self.entry_bytes_remaining;
        e.mtime = tar_atol(&h[136..148]);
        let typeflag = h[156];
        match typeflag {
            b'1' => {
                e.hardlink = Some(linkname);
                if e.size > 0 {
                    e.set_filetype(S_IFREG);
                }
                // Old writers put sizes on hard links without a body: trust
                // the size only in pax archives, or when no valid header
                // follows the entry (libarchive's heuristic).
                let ignore_size = e.size != 0
                    && self.format != Format::Pax
                    && (self.format == Format::Gnu || {
                        let next = self.input.peek(BLOCK).map_err(io_err)?;
                        tar_bid(&next) > 50
                    });
                if ignore_size {
                    e.size = 0;
                    self.entry_bytes_remaining = 0;
                }
            }
            b'2' => {
                e.set_filetype(S_IFLNK);
                e.size = 0;
                self.entry_bytes_remaining = 0;
                e.symlink = Some(linkname);
            }
            b'3' => {
                e.set_filetype(S_IFCHR);
                e.size = 0;
                self.entry_bytes_remaining = 0;
            }
            b'4' => {
                e.set_filetype(S_IFBLK);
                e.size = 0;
                self.entry_bytes_remaining = 0;
            }
            b'5' => {
                e.set_filetype(S_IFDIR);
                e.size = 0;
                self.entry_bytes_remaining = 0;
            }
            b'6' => {
                e.set_filetype(S_IFIFO);
                e.size = 0;
                self.entry_bytes_remaining = 0;
            }
            b'D' => e.set_filetype(S_IFDIR),
            b'M' => {}
            _ => e.set_filetype(S_IFREG),
        }
        if typeflag == b'3' || typeflag == b'4' {
            e.devmajor = tar_atol(&h[329..337]);
            e.devminor = tar_atol(&h[337..345]);
        }

        if kind == Format::Gnu {
            let t = tar_atol(&h[345..357]);
            if t > 0 {
                e.atime = t;
            }
            let t = tar_atol(&h[357..369]);
            if t > 0 {
                e.ctime = t;
            }
            if h[483] != 0 {
                let rs = tar_atol(&h[483..495]);
                self.realsize = rs;
                e.size = rs;
                sparse.realsize_override = Some(rs);
            }
            if h[386] != 0 {
                // old GNU sparse map: 4 entries in the header...
                self.parse_old_sparse(&h[386..482], 4, sparse);
                let mut extended = h[482] != 0;
                // ...plus extension blocks of 21 entries
                while extended {
                    let mut ext = [0u8; BLOCK];
                    if self.read_block(&mut ext)? < BLOCK {
                        return Err(header_err("Truncated tar archive"));
                    }
                    self.parse_old_sparse(&ext[..504], 21, sparse);
                    extended = ext[504] != 0;
                }
            }
        }
        Ok(e)
    }

    fn parse_old_sparse(&mut self, area: &[u8], count: usize, sparse: &mut SparseInfo) {
        for i in 0..count {
            let s = &area[i * 24..i * 24 + 24];
            if s[0] == 0 {
                break;
            }
            sparse
                .entries
                .push((tar_atol(&s[0..12]), tar_atol(&s[12..24])));
        }
    }

    /// Apply a pax extended header (libarchive's `pax_header`).
    fn apply_pax(&mut self, e: &mut TarEntry, text: &[u8], sparse: &mut SparseInfo) -> Result<()> {
        let mut path: Option<Vec<u8>> = None;
        let mut linkpath: Option<Vec<u8>> = None;
        let mut rest = text;
        while !rest.is_empty() {
            // "%d key=value\n"
            let sp = rest
                .iter()
                .position(|&c| c == b' ')
                .ok_or_else(|| header_err("Invalid pax extended attributes"))?;
            let len: usize = std::str::from_utf8(&rest[..sp])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| header_err("Invalid pax extended attributes"))?;
            if len <= sp + 1 || len > rest.len() || rest[len - 1] != b'\n' {
                return Err(header_err("Invalid pax extended attributes"));
            }
            let record = &rest[sp + 1..len - 1];
            rest = &rest[len..];
            let eq = record
                .iter()
                .position(|&c| c == b'=')
                .ok_or_else(|| header_err("Invalid pax extended attributes"))?;
            let (key, value) = (&record[..eq], &record[eq + 1..]);
            match key {
                b"path" => path = Some(value.to_vec()),
                b"linkpath" => linkpath = Some(value.to_vec()),
                b"uid" => e.uid = tar_atol10(value),
                b"gid" => e.gid = tar_atol10(value),
                b"mtime" => e.mtime = pax_time(value),
                b"ctime" => e.ctime = pax_time(value),
                b"atime" => e.atime = pax_time(value),
                b"size" => {
                    self.entry_bytes_remaining = tar_atol10(value);
                    if sparse.realsize_override.is_none() {
                        e.size = self.entry_bytes_remaining;
                        self.realsize = self.entry_bytes_remaining;
                    }
                }
                b"SCHILY.devmajor" => e.devmajor = tar_atol10(value),
                b"SCHILY.devminor" => e.devminor = tar_atol10(value),
                b"SCHILY.realsize" | b"GNU.sparse.realsize" | b"GNU.sparse.size" => {
                    let rs = tar_atol10(value);
                    self.realsize = rs;
                    e.size = rs;
                    sparse.realsize_override = Some(rs);
                }
                b"GNU.sparse.name" => sparse.name_override = Some(value.to_vec()),
                b"GNU.sparse.major" => sparse.gnu_major = Some(tar_atol10(value)),
                b"GNU.sparse.minor" => sparse.gnu_minor = Some(tar_atol10(value)),
                b"GNU.sparse.numblocks" => {
                    sparse.entries.clear();
                    sparse.gnu_major = Some(0);
                    sparse.gnu_minor = Some(0);
                }
                b"GNU.sparse.offset" => sparse.pending_offset = Some(tar_atol10(value)),
                b"GNU.sparse.numbytes" => {
                    if let Some(off) = sparse.pending_offset.take() {
                        sparse.entries.push((off, tar_atol10(value)));
                    }
                }
                b"GNU.sparse.map" => {
                    sparse.entries.clear();
                    sparse.gnu_major = Some(0);
                    sparse.gnu_minor = Some(1);
                    let nums: Vec<i64> = value.split(|&c| c == b',').map(tar_atol10).collect();
                    for pair in nums.chunks_exact(2) {
                        sparse.entries.push((pair[0], pair[1]));
                    }
                }
                _ => {}
            }
        }
        if let Some(l) = linkpath.filter(|l| !l.is_empty()) {
            e.set_link(l);
        }
        if let Some(n) = sparse.name_override.clone().filter(|n| !n.is_empty()) {
            e.path = n;
        } else if let Some(p) = path.filter(|p| !p.is_empty()) {
            e.path = p;
        }
        Ok(())
    }

    /// Final adjustments once all headers are known.
    fn finish_entry(&mut self, e: &mut TarEntry, sparse: SparseInfo) -> Result<()> {
        // pax 1.0 sparse: the map is stored at the start of the data
        if sparse.gnu_major == Some(1) && sparse.gnu_minor == Some(0) {
            let (consumed, segs) = self.read_sparse_10_map()?;
            self.entry_bytes_remaining -= consumed;
            self.segments = segs;
        } else if sparse.gnu_major.is_some() && sparse.gnu_major != Some(0) {
            return Err(header_err("Unrecognized GNU sparse file format"));
        } else if !sparse.entries.is_empty() {
            self.segments = sparse.entries.iter().copied().collect();
        }
        self.padding = 0x1ff & self.entry_bytes_remaining.wrapping_neg();
        if self.segments.is_empty() {
            self.segments.push_back((0, self.entry_bytes_remaining));
        }

        // "Regular" entry with trailing '/' is really a directory.
        if e.file_type() == S_IFREG && e.path.last() == Some(&b'/') {
            e.set_filetype(S_IFDIR);
            self.entry_bytes_remaining = 0;
            self.padding = 0;
            self.segments.clear();
        }
        Ok(())
    }

    /// Read a GNU sparse 1.0 map stored at the start of the entry data;
    /// returns (bytes consumed, segments).
    fn read_sparse_10_map(&mut self) -> Result<(i64, VecDeque<(i64, i64)>)> {
        let mut data: Vec<u8> = Vec::new();
        let mut pos = 0usize;
        let mut consumed: i64 = 0;
        let mut values = Vec::new();
        let mut wanted: Option<i64> = None;
        loop {
            if let Some(n) = wanted {
                if values.len() as i64 == 2 * n {
                    break;
                }
            }
            match data[pos..].iter().position(|&c| c == b'\n') {
                Some(nl) => {
                    let line = &data[pos..pos + nl];
                    if line.is_empty() || !line.iter().all(|c| c.is_ascii_digit()) {
                        return Err(header_err("Malformed GNU sparse map"));
                    }
                    let v = tar_atol10(line);
                    pos += nl + 1;
                    if wanted.is_none() {
                        wanted = Some(v);
                    } else {
                        values.push(v);
                    }
                }
                None => {
                    let mut blk = [0u8; BLOCK];
                    if self.read_block(&mut blk)? < BLOCK {
                        return Err(header_err("Truncated tar archive"));
                    }
                    consumed += BLOCK as i64;
                    data.extend_from_slice(&blk);
                }
            }
        }
        let segs = values.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        Ok((consumed, segs))
    }

    /// One block of entry data (libarchive's `read_data` for tar):
    /// returns (logical offset, length) or None at the end of the entry.
    fn data_block(&mut self) -> Option<(i64, i64)> {
        while let Some(&(_, rem)) = self.segments.front() {
            if rem != 0 {
                break;
            }
            self.segments.pop_front();
        }
        if self.segments.is_empty() || self.entry_bytes_remaining <= 0 {
            return None;
        }
        let (off, rem) = self.segments.front_mut().unwrap();
        let n = (*rem).min(self.entry_bytes_remaining);
        let offset = *off;
        *off += n;
        *rem -= n;
        self.entry_bytes_remaining -= n;
        Some((offset, n))
    }

    /// `archive_read_data`: fill `buf` with entry data (holes as zeros).
    /// Like libarchive, returns early (possibly short) when the stored data
    /// ends, and pads a trailing hole on subsequent calls.
    pub fn read_data(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut s = buf.len();
        let mut dest = 0usize;
        let mut bytes_read = 0usize;
        while s > 0 {
            if self.rd_offset == self.rd_output && self.rd_remaining == 0 {
                match self.data_block() {
                    None => {
                        // end of stored data: a trailing hole is padded on
                        // the next call, exactly like libarchive
                        self.rd_offset = self.realsize;
                        return Ok(bytes_read);
                    }
                    Some((off, len)) => {
                        self.rd_offset = off;
                        self.rd_remaining = len;
                    }
                }
            }
            if self.rd_offset < self.rd_output {
                return Err(Error::fatal("Encountered out-of-order sparse blocks"));
            }
            let gap = self.rd_offset - self.rd_output;
            let len = if gap as u64 > s as u64 {
                s
            } else {
                gap as usize
            };
            buf[dest..dest + len].fill(0);
            s -= len;
            self.rd_output += len as i64;
            dest += len;
            bytes_read += len;
            if s > 0 {
                let len = (self.rd_remaining as u64).min(s as u64) as usize;
                if len > 0 {
                    let got = self
                        .input
                        .read_full(&mut buf[dest..dest + len])
                        .map_err(|e| Error::io("archive_read_data()", e))?;
                    if got < len {
                        return Err(Error::fatal("Truncated tar archive"));
                    }
                    dest += len;
                    s -= len;
                    bytes_read += len;
                    self.rd_remaining -= len as i64;
                    self.rd_offset += len as i64;
                    self.rd_output += len as i64;
                }
            }
        }
        Ok(bytes_read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, typeflag: u8, size: u64, gnu: bool) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0001750");
        h[116..123].copy_from_slice(b"0001750");
        let sz = format!("{size:011o}");
        h[124..135].copy_from_slice(sz.as_bytes());
        h[136..147].copy_from_slice(b"14000000000");
        h[156] = typeflag;
        if gnu {
            h[257..265].copy_from_slice(b"ustar  \0");
        } else {
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
        }
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        let cs = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(cs.as_bytes());
        h
    }

    fn archive(parts: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
            let pad = (BLOCK - p.len() % BLOCK) % BLOCK;
            v.extend(std::iter::repeat_n(0u8, pad));
        }
        v.extend_from_slice(&[0u8; 1024]);
        v
    }

    #[test]
    fn basic_entries() {
        let data = archive(&[
            &header("dir/", b'5', 0, false),
            &header("dir/file", b'0', 5, false),
            b"hello",
            &{
                let mut h = header("dir/link", b'1', 0, false);
                h[157..165].copy_from_slice(b"dir/file");
                h[148..156].copy_from_slice(b"        ");
                let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
                h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
                h
            },
        ]);
        let mut r = TarReader::from_reader(Box::new(io::Cursor::new(data))).unwrap();
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.path, b"dir/");
        assert_eq!(e.file_type(), S_IFDIR);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.path, b"dir/file");
        assert_eq!(e.file_type(), S_IFREG);
        assert_eq!(e.size, 5);
        let mut buf = [0u8; 16];
        assert_eq!(r.read_data(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(r.read_data(&mut buf).unwrap(), 0);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.file_type(), 0);
        assert_eq!(e.hardlink.as_deref(), Some(&b"dir/file"[..]));
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn long_names_and_pax() {
        let long = "a/".repeat(80) + "file";
        let mut pax = Vec::new();
        let rec = |k: &str, v: &str| {
            let body = format!(" {k}={v}\n");
            let mut n = body.len() + 1;
            loop {
                let s = format!("{n}{body}");
                if s.len() == n {
                    return s;
                }
                n = s.len();
            }
        };
        pax.extend_from_slice(rec("path", &long).as_bytes());
        pax.extend_from_slice(rec("mtime", "1700000000.5").as_bytes());
        pax.extend_from_slice(rec("uid", "123456").as_bytes());
        let mut gnu_name = long.clone().into_bytes();
        gnu_name.push(0);
        let data = archive(&[
            &header("././@PaxHeader", b'x', pax.len() as u64, false),
            &pax,
            &header("short", b'0', 0, false),
            &header("././@LongLink", b'L', gnu_name.len() as u64, true),
            &gnu_name,
            &header("trunc", b'0', 0, true),
        ]);
        let mut r = TarReader::from_reader(Box::new(io::Cursor::new(data))).unwrap();
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.path, long.as_bytes());
        assert_eq!(e.mtime, 1_700_000_000);
        assert_eq!(e.uid, 123456);
        let e = r.next_entry().unwrap().unwrap();
        assert_eq!(e.path, long.as_bytes());
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn empty_input_is_empty_archive() {
        let mut r = TarReader::from_reader(Box::new(io::Cursor::new(Vec::new()))).unwrap();
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn garbage_is_rejected() {
        let mut r = TarReader::from_reader(Box::new(io::Cursor::new(vec![b'x'; 2048]))).unwrap();
        assert!(r.next_entry().is_err());
    }

    #[test]
    fn numbers() {
        assert_eq!(tar_atol(b"0000644\0"), 0o644);
        assert_eq!(tar_atol(b"  17 "), 0o17);
        let mut b256 = [0u8; 12];
        b256[0] = 0x80;
        b256[11] = 5;
        assert_eq!(tar_atol(&b256), 5);
        assert_eq!(pax_time(b"-12.5"), -12);
    }
}
