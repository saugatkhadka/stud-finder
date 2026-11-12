//! Sidecar format (little-endian), version 1:
//!
//! ```text
//! "RPLHIST\0"  u32 version
//! header:  str head_oid
//!          u64 max_commits, i64 since (i64::MIN = none), u64 max_files_per_commit,
//!          u32 min_together, u64 keep_per_file, u32 n, n × str ignore
//! paths:   u32 bytes, names, u32 n+1, name_off[n+1]
//! commits: u32 n, n × (str oid, i64 time, u32 raw_files), tx_off[n+1], u32 m, tx[m]
//! churn:   u32 n, n × (u32 changes, u32 pair_changes, u32 last)
//! adj:     adj_off[files+1], u32 m, m × (u32 other, u32 together, u32 last)
//!          u32 pair_commits
//! u64 checksum of everything before it
//! ```
//!
//! `str` = u32 length + UTF-8 bytes. Arrays of u32 are written raw, so a
//! load is one read plus bulk copies.

use crate::{CommitMeta, History, Options, Pair};
use anyhow::{Result, bail, ensure};

const MAGIC: &[u8; 8] = b"RPLHIST\0";
pub(crate) const VERSION: u32 = 1;

fn checksum(b: &[u8]) -> u64 {
    // Word-wise multiply-xor: cheap and good enough to catch truncation and corruption.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ b.len() as u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().expect("8"))).wrapping_mul(0x0100_0000_01b3).rotate_left(23);
    }
    for &x in chunks.remainder() {
        h = (h ^ x as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

struct W(Vec<u8>);
impl W {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn u32s(&mut self, v: &[u32]) {
        self.u32(v.len() as u32);
        self.0.reserve(v.len() * 4);
        for &x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
}

pub(crate) fn encode(h: &History) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(64 + h.names.len() + 4 * (h.name_off.len() + h.tx.len() + h.tx_off.len() + 3 * h.churn.len() + h.adj_off.len() + 3 * h.adj.len()) + 64 * h.commits.len()));
    w.0.extend_from_slice(MAGIC);
    w.u32(VERSION);
    w.str(&h.head);
    let o = &h.opts;
    w.u64(o.max_commits as u64);
    w.i64(o.since.unwrap_or(i64::MIN));
    w.u64(o.max_files_per_commit as u64);
    w.u32(o.min_together);
    w.u64(o.keep_per_file as u64);
    w.u32(o.ignore.len() as u32);
    for g in &o.ignore {
        w.str(g);
    }
    w.str(&h.names);
    w.u32s(&h.name_off);
    w.u32(h.commits.len() as u32);
    for c in &h.commits {
        w.str(&c.oid);
        w.i64(c.time);
        w.u32(c.raw_files);
    }
    w.u32s(&h.tx_off);
    w.u32s(&h.tx);
    w.u32(h.churn.len() as u32);
    for &(a, b, c) in &h.churn {
        w.u32(a);
        w.u32(b);
        w.u32(c);
    }
    w.u32s(&h.adj_off);
    w.u32(h.adj.len() as u32);
    for p in &h.adj {
        w.u32(p.other);
        w.u32(p.together);
        w.u32(p.last);
    }
    w.u32(h.pair_commits);
    let sum = checksum(&w.0);
    w.u64(sum);
    w.0
}

struct R<'a> {
    b: &'a [u8],
    at: usize,
}
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.at + n <= self.b.len(), "truncated");
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(std::str::from_utf8(self.take(n)?)?.to_string())
    }
    fn u32s(&mut self) -> Result<Vec<u32>> {
        let n = self.u32()? as usize;
        Ok(self.take(n.checked_mul(4).ok_or_else(|| anyhow::anyhow!("bad length"))?)?.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().expect("4"))).collect())
    }
    fn triples(&mut self) -> Result<Vec<(u32, u32, u32)>> {
        let n = self.u32()? as usize;
        Ok(self
            .take(n.checked_mul(12).ok_or_else(|| anyhow::anyhow!("bad length"))?)?
            .chunks_exact(12)
            .map(|c| (u32::from_le_bytes(c[0..4].try_into().expect("4")), u32::from_le_bytes(c[4..8].try_into().expect("4")), u32::from_le_bytes(c[8..12].try_into().expect("4"))))
            .collect())
    }
}

pub(crate) fn decode(b: &[u8]) -> Result<History> {
    ensure!(b.len() >= 20 && &b[..8] == MAGIC, "not a ripple history file");
    let (body, tail) = b.split_at(b.len() - 8);
    ensure!(checksum(body) == u64::from_le_bytes(tail.try_into()?), "checksum mismatch (truncated or corrupt)");
    let mut r = R { b: body, at: 8 };
    let version = r.u32()?;
    if version != VERSION {
        bail!("format version {version}, expected {VERSION}");
    }
    let head = r.str()?;
    let max_commits = r.u64()? as usize;
    let since = Some(r.i64()?).filter(|&s| s != i64::MIN);
    let max_files_per_commit = r.u64()? as usize;
    let min_together = r.u32()?;
    let keep_per_file = r.u64()? as usize;
    let n = r.u32()? as usize;
    let ignore = (0..n).map(|_| r.str()).collect::<Result<Vec<_>>>()?;
    let opts = Options { max_commits, since, max_files_per_commit, min_together, keep_per_file, ignore };
    let names = r.str()?;
    let name_off = r.u32s()?;
    let nc = r.u32()? as usize;
    let mut commits = Vec::with_capacity(nc);
    for _ in 0..nc {
        commits.push(CommitMeta { oid: r.str()?, time: r.i64()?, raw_files: r.u32()? });
    }
    let tx_off = r.u32s()?;
    let tx = r.u32s()?;
    let churn = r.triples()?;
    let adj_off = r.u32s()?;
    let adj: Vec<Pair> = r.triples()?.into_iter().map(|(other, together, last)| Pair { other, together, last }).collect();
    let pair_commits = r.u32()?;
    ensure!(r.at == body.len(), "trailing bytes");
    // Structural checks so a bad file can never make a query panic.
    let files = name_off.len().saturating_sub(1);
    ensure!(!name_off.is_empty() && name_off.windows(2).all(|w| w[0] <= w[1]) && *name_off.last().expect("nonempty") as usize == names.len(), "bad path table");
    ensure!(name_off.iter().all(|&o| names.is_char_boundary(o as usize)), "bad path table");
    ensure!(tx_off.len() == nc + 1 && tx_off.windows(2).all(|w| w[0] <= w[1]) && tx_off[nc] as usize == tx.len() && tx.iter().all(|&f| (f as usize) < files), "bad commits");
    ensure!(churn.len() == files && churn.iter().all(|c| (c.2 as usize) < nc), "bad churn");
    ensure!(adj_off.len() == files + 1 && adj_off.windows(2).all(|w| w[0] <= w[1]) && adj_off[files] as usize == adj.len(), "bad partner lists");
    ensure!(adj.iter().all(|p| (p.other as usize) < files && (p.last as usize) < nc), "bad partner lists");
    Ok(History { head, opts, names, name_off, commits, tx_off, tx, churn, adj_off, adj, pair_commits, recent: Vec::new() }.derive())
}
