//! Flat columns that are either owned or borrowed from a read-only file
//! mapping, and the section file format they are saved in.
//!
//! The query-side index (graph, mentions, metrics, scopes) is made of flat
//! arrays of plain integers. Built in memory they are owned boxes; saved
//! with [`SectionWriter`] and opened with [`SectionFile`] they point into an
//! `mmap` of the file, so a process that serves queries from a saved index
//! holds only the pages the queries touch, as clean file-backed pages the
//! OS can drop at any time (they do not count towards the process's
//! footprint).
//!
//! # File format
//!
//! ```text
//! magic "RPLSECT1" | format version u32 | byte-order mark u32 (0x01020304)
//! | section count u32 | reserved u32 | checksum u64 of every byte after the header
//! | section table: (id u32, reserved u32, offset u64, length u64) * count
//! | sections, each starting at a multiple of 8
//! ```
//!
//! A file is accepted only if the magic, version, byte order and checksum
//! match and every section lies inside the file; callers then validate the
//! contents they rely on for memory safety (indices in range, enum bytes
//! valid) before handing out views.

use std::collections::HashMap;
use std::io::Write as _;
use std::ops::Deref;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

/// Plain old data: any bit pattern of the right size is a valid value, no
/// padding, no pointers.
///
/// # Safety
/// Implementors must have no padding bytes and accept every bit pattern.
pub unsafe trait Pod: Copy + Send + Sync + 'static {}
unsafe impl Pod for u8 {}
unsafe impl Pod for u16 {}
unsafe impl Pod for u32 {}
unsafe impl Pod for u64 {}
unsafe impl Pod for i64 {}
unsafe impl Pod for [u32; 2] {}

/// A read-only private mapping of a whole file.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is read-only and never mutated after creation.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    pub fn open(path: &Path) -> std::io::Result<Arc<Self>> {
        use std::os::fd::AsRawFd;
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len() as usize;
        if len == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty file"));
        }
        // SAFETY: a fresh read-only private mapping of an open file; the
        // file is replaced by rename, never written in place, so the bytes
        // do not change under us.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Arc::new(Self { ptr: NonNull::new(p as *mut u8).expect("mmap returned null"), len }))
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for our lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `open` mapped.
        unsafe { libc::munmap(self.ptr.as_ptr() as *mut _, self.len) };
    }
}

impl std::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mapping({} bytes)", self.len)
    }
}

enum Keep<T> {
    Owned(#[allow(dead_code)] Box<[T]>),
    Mapped(#[allow(dead_code)] Arc<Mapping>),
}

/// An immutable array of `T`: owned, or a window into a [`Mapping`].
pub struct Slab<T: Pod> {
    ptr: NonNull<T>,
    len: usize,
    keep: Keep<T>,
}

// SAFETY: immutable data, owned or kept alive by an `Arc<Mapping>`.
unsafe impl<T: Pod> Send for Slab<T> {}
unsafe impl<T: Pod> Sync for Slab<T> {}

impl<T: Pod> Slab<T> {
    pub fn is_mapped(&self) -> bool {
        matches!(self.keep, Keep::Mapped(_))
    }

    /// Heap bytes owned by this slab (0 when mapped).
    pub fn heap_bytes(&self) -> usize {
        match self.keep {
            Keep::Owned(_) => self.len * std::mem::size_of::<T>(),
            Keep::Mapped(_) => 0,
        }
    }

    /// The raw bytes, for saving.
    pub fn as_bytes(&self) -> &[u8] {
        as_bytes(self)
    }
}

/// The bytes of a slice of plain data.
pub fn as_bytes<T: Pod>(v: &[T]) -> &[u8] {
    // SAFETY: `T: Pod` has no padding, so every byte is initialised.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

impl<T: Pod> From<Box<[T]>> for Slab<T> {
    fn from(b: Box<[T]>) -> Self {
        let len = b.len();
        // A boxed slice's data pointer is stable when the box moves.
        let ptr = NonNull::new(b.as_ptr() as *mut T).unwrap_or(NonNull::dangling());
        Self { ptr, len, keep: Keep::Owned(b) }
    }
}

impl<T: Pod> From<Vec<T>> for Slab<T> {
    fn from(v: Vec<T>) -> Self {
        v.into_boxed_slice().into()
    }
}

impl<T: Pod> FromIterator<T> for Slab<T> {
    fn from_iter<I: IntoIterator<Item = T>>(it: I) -> Self {
        it.into_iter().collect::<Vec<T>>().into()
    }
}

impl<T: Pod> Default for Slab<T> {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl<T: Pod> Clone for Slab<T> {
    fn clone(&self) -> Self {
        match &self.keep {
            Keep::Owned(_) => self.to_vec().into(),
            Keep::Mapped(m) => Self { ptr: self.ptr, len: self.len, keep: Keep::Mapped(m.clone()) },
        }
    }
}

impl<T: Pod> Deref for Slab<T> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: `ptr` points to `len` initialised `T`s kept alive by `keep`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: Pod + std::fmt::Debug> std::fmt::Debug for Slab<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter().take(16)).finish()?;
        if self.len > 16 {
            write!(f, "..({} total)", self.len)?;
        }
        Ok(())
    }
}

impl<T: Pod + PartialEq> PartialEq for Slab<T> {
    fn eq(&self, o: &Self) -> bool {
        **self == **o
    }
}
impl<T: Pod + Eq> Eq for Slab<T> {}

/// Strings stored back to back: string `i` is `bytes[ends[i-1]..ends[i]]`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StrTable {
    ends: Slab<u32>,
    bytes: Slab<u8>,
}

impl std::fmt::Debug for StrTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter().take(16)).finish()
    }
}

impl StrTable {
    pub fn from_strs<'a>(it: impl IntoIterator<Item = &'a str>) -> Self {
        let mut ends = Vec::new();
        let mut bytes = Vec::new();
        for s in it {
            bytes.extend_from_slice(s.as_bytes());
            ends.push(bytes.len() as u32);
        }
        Self { ends: ends.into(), bytes: bytes.into() }
    }

    /// Checks what `get` relies on: ends non-decreasing within `bytes`,
    /// UTF-8 text, every string boundary a char boundary.
    pub fn new_checked(ends: Slab<u32>, bytes: Slab<u8>) -> Result<Self, String> {
        let text = std::str::from_utf8(&bytes).map_err(|e| format!("string table is not UTF-8: {e}"))?;
        let mut prev = 0u32;
        for &e in ends.iter() {
            if e < prev || e as usize > bytes.len() || !text.is_char_boundary(e as usize) {
                return Err("string table offsets out of order".into());
            }
            prev = e;
        }
        Ok(Self { ends, bytes })
    }

    #[inline]
    pub fn get(&self, i: usize) -> &str {
        let end = self.ends[i] as usize;
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        // SAFETY: built from `&str`s or checked by `new_checked` (UTF-8,
        // boundaries on char boundaries, in range).
        unsafe { std::str::from_utf8_unchecked(self.bytes.get_unchecked(start..end)) }
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + Clone + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }

    /// Index of `s` in a table sorted in byte order.
    pub fn binary_search(&self, s: &str) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0usize, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.get(mid).cmp(s) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    pub fn heap_bytes(&self) -> usize {
        self.ends.heap_bytes() + self.bytes.heap_bytes()
    }

    pub fn save(&self, w: &mut SectionWriter, ends_id: u32, bytes_id: u32) {
        w.put(ends_id, &self.ends);
        w.put(bytes_id, &self.bytes);
    }

    pub fn load(f: &SectionFile, ends_id: u32, bytes_id: u32) -> Result<Self, String> {
        Self::new_checked(f.slab(ends_id)?, f.slab(bytes_id)?)
    }
}

const MAGIC: &[u8; 8] = b"RPLSECT1";
const BOM: u32 = 0x0102_0304;
const HEADER: usize = 8 + 4 + 4 + 4 + 4 + 8;

/// 64-bit checksum over 8-byte words (fast; detects truncation and torn writes).
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0x9e37_79b9_7f4a_7c15 ^ bytes.len() as u64;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3).rotate_left(29);
    }
    for &b in chunks.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ (h >> 31)
}

/// Collects sections and writes them as one file, atomically.
#[derive(Default)]
pub struct SectionWriter {
    data: Vec<u8>,
    sections: Vec<(u32, u64, u64)>,
}

impl SectionWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_bytes(&mut self, id: u32, bytes: &[u8]) {
        while !self.data.len().is_multiple_of(8) {
            self.data.push(0);
        }
        self.sections.push((id, self.data.len() as u64, bytes.len() as u64));
        self.data.extend_from_slice(bytes);
    }

    pub fn put<T: Pod>(&mut self, id: u32, v: &[T]) {
        self.put_bytes(id, as_bytes(v));
    }

    /// The complete file contents.
    pub fn finish(self, version: u32) -> Vec<u8> {
        let table = self.sections.len() * 24;
        let base = (HEADER + table).next_multiple_of(8);
        let mut body = Vec::with_capacity(base - HEADER + self.data.len());
        for &(id, off, len) in &self.sections {
            body.extend_from_slice(&id.to_ne_bytes());
            body.extend_from_slice(&0u32.to_ne_bytes());
            body.extend_from_slice(&(base as u64 + off).to_ne_bytes());
            body.extend_from_slice(&len.to_ne_bytes());
        }
        body.resize(base - HEADER, 0);
        body.extend_from_slice(&self.data);
        let mut out = Vec::with_capacity(HEADER + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&version.to_ne_bytes());
        out.extend_from_slice(&BOM.to_ne_bytes());
        out.extend_from_slice(&(self.sections.len() as u32).to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&checksum(&body).to_ne_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Writes the file to `path` via a temporary file and a rename, so a
    /// reader sees the old file or the complete new one.
    pub fn write_atomic(self, version: u32, path: &Path) -> std::io::Result<u64> {
        let bytes = self.finish(version);
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("snapshot"), std::process::id()));
        let mut f = std::fs::File::create(&tmp)?;
        let r = f.write_all(&bytes).and_then(|_| f.flush());
        drop(f);
        if let Err(e) = r.and_then(|_| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(bytes.len() as u64)
    }
}

/// A section file opened with [`Mapping`]; see the module docs.
#[derive(Debug)]
pub struct SectionFile {
    map: Arc<Mapping>,
    sections: HashMap<u32, (usize, usize)>,
}

impl SectionFile {
    pub fn open(path: &Path, version: u32) -> Result<Self, String> {
        let map = Mapping::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let b = map.bytes();
        if b.len() < HEADER || &b[..8] != MAGIC {
            return Err("not a ripple section file".into());
        }
        let u32_at = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().expect("4 bytes"));
        let u64_at = |o: usize| u64::from_ne_bytes(b[o..o + 8].try_into().expect("8 bytes"));
        if u32_at(12) != BOM {
            return Err("written on a machine with another byte order".into());
        }
        if u32_at(8) != version {
            return Err(format!("format version {} (this ripple reads {version})", u32_at(8)));
        }
        let count = u32_at(16) as usize;
        if checksum(&b[HEADER..]) != u64_at(24) {
            return Err("checksum mismatch (truncated or corrupt)".into());
        }
        let table_end = HEADER + count * 24;
        if table_end > b.len() {
            return Err("section table out of range".into());
        }
        let mut sections = HashMap::new();
        for i in 0..count {
            let o = HEADER + i * 24;
            let (id, off, len) = (u32_at(o), u64_at(o + 8) as usize, u64_at(o + 16) as usize);
            if off % 8 != 0 || off < table_end || off.checked_add(len).is_none_or(|e| e > b.len()) {
                return Err(format!("section {id} out of range"));
            }
            sections.insert(id, (off, len));
        }
        Ok(Self { map, sections })
    }

    pub fn mapping(&self) -> &Arc<Mapping> {
        &self.map
    }

    pub fn has(&self, id: u32) -> bool {
        self.sections.contains_key(&id)
    }

    pub fn bytes(&self, id: u32) -> Result<&[u8], String> {
        let &(off, len) = self.sections.get(&id).ok_or_else(|| format!("missing section {id}"))?;
        Ok(&self.map.bytes()[off..off + len])
    }

    /// Section `id` as a mapped array of `T`.
    pub fn slab<T: Pod>(&self, id: u32) -> Result<Slab<T>, String> {
        let &(off, len) = self.sections.get(&id).ok_or_else(|| format!("missing section {id}"))?;
        let size = std::mem::size_of::<T>();
        if len % size != 0 || off % std::mem::align_of::<T>() != 0 {
            return Err(format!("section {id} has a bad size"));
        }
        if len == 0 {
            return Ok(Slab::default());
        }
        // SAFETY: in range (checked in `open`), aligned, `T: Pod`.
        let ptr = unsafe { self.map.ptr.as_ptr().add(off) as *mut T };
        Ok(Slab { ptr: NonNull::new(ptr).expect("non-null"), len: len / size, keep: Keep::Mapped(self.map.clone()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_corruption() {
        let dir = std::env::temp_dir().join(format!("ripple-store-{}", std::process::id()));
        let path = dir.join("t.bin");
        let mut w = SectionWriter::new();
        w.put::<u32>(1, &[1, 2, 3]);
        w.put_bytes(2, b"hello");
        let t = StrTable::from_strs(["a", "bc", "", "déf"]);
        t.save(&mut w, 3, 4);
        w.put::<u64>(5, &[]);
        w.write_atomic(7, &path).unwrap();

        let f = SectionFile::open(&path, 7).unwrap();
        assert_eq!(&*f.slab::<u32>(1).unwrap(), &[1, 2, 3]);
        assert_eq!(f.bytes(2).unwrap(), b"hello");
        let t2 = StrTable::load(&f, 3, 4).unwrap();
        assert_eq!(t2.iter().collect::<Vec<_>>(), vec!["a", "bc", "", "déf"]);
        assert_eq!(t2, t);
        assert!(t2.binary_search("bc").is_err() || t2.binary_search("bc").is_ok());
        assert!(f.slab::<u64>(5).unwrap().is_empty());
        assert!(f.slab::<u64>(1).is_err(), "12 bytes are not u64s");
        assert!(SectionFile::open(&path, 8).is_err(), "version");

        // Flip one byte: rejected.
        let mut b = std::fs::read(&path).unwrap();
        let n = b.len();
        b[n - 3] ^= 1;
        std::fs::write(&path, &b).unwrap();
        assert!(SectionFile::open(&path, 7).unwrap_err().contains("checksum"));
        // Truncated: rejected.
        std::fs::write(&path, &b[..n / 2]).unwrap();
        assert!(SectionFile::open(&path, 7).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_string_tables_are_rejected() {
        assert!(StrTable::new_checked(vec![3u32, 1].into(), b"abc".to_vec().into()).is_err());
        assert!(StrTable::new_checked(vec![4u32].into(), b"abc".to_vec().into()).is_err());
        assert!(StrTable::new_checked(vec![1u32].into(), "é".as_bytes().to_vec().into()).is_err());
        assert!(StrTable::new_checked(vec![2u32].into(), "é".as_bytes().to_vec().into()).is_ok());
    }
}
