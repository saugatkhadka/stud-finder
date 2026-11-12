//! ripple-coverage: line coverage from test-coverage reports that already
//! exist. ripple never runs tests; it reads what CI or a local test run
//! wrote, so it can say "the lines you changed are 40% covered" or "method
//! `foo` is not covered by any test".
//!
//! A separate signal, like `ripple-history`: never a graph edge.
//!
//! # Formats
//!
//! * **SimpleCov** `coverage/.resultset.json`, both the legacy
//!   `{"coverage": {file: [hits|null, ..]}}` form and the 0.18+
//!   `{"coverage": {file: {"lines": [..], "branches": ..}}}` form. Several
//!   suites (commands) in one resultset are merged.
//! * **LCOV** `coverage/lcov.info`, `coverage/lcov/*.lcov` (Jest, Vitest,
//!   c8, simplecov-lcov): `SF`/`DA`/`end_of_record`. `LF`/`LH` are ignored
//!   (totals are recomputed from `DA`), as are `FN*` and `BR*`.
//! * **Istanbul** `coverage/coverage-final.json`: a line is coverable when a
//!   statement starts on it and covered when one of those statements ran,
//!   the way Istanbul's own LCOV reporter derives line coverage.
//!
//! Only line coverage is kept. Branch and function data are not read.
//!
//! # Merging
//!
//! All reports found are merged per file and line: a line is coverable if
//! any measurement says so (`null` = not coverable) and covered if the
//! summed hits are positive, i.e. if any run hit it. Hit counts are not
//! kept. Reading the same data twice (Jest's `lcov.info` and
//! `coverage-final.json`) is therefore harmless.
//!
//! # Path mapping
//!
//! Report paths are absolute, absolute in another checkout (CI:
//! `/builds/org/repo/app/x.rb`), or relative to a JS package root. For each
//! report:
//!
//! 1. An absolute path under the repo root (as given or canonicalized) is
//!    stripped to repo-relative. A relative path is tried relative to the
//!    report's project directory (the parent of its `coverage/` directory),
//!    then relative to the repo root. If the result is a known file it is
//!    used; an absolute path under the root that is not a known file is
//!    unmapped (deleted since).
//! 2. Otherwise by path suffix against the known files: candidates share the
//!    file name, and one path must be entirely a component-wise suffix of the
//!    other (`/ci/app/x.rb` fits `app/x.rb`, not `ee/app/x.rb`). The longest
//!    match wins; equally long matches are ambiguous.
//! 3. Unique suffix matches vote for a rewrite "strip prefix A, prepend B"
//!    (A = the other checkout root, B = a package directory). The most
//!    common rewrite is applied first to every path from step 2. A path
//!    under A whose rewrite is not a known file is unmapped (rather than
//!    matched to a namesake), which also resolves most ambiguous cases.
//!
//! Entries that remain unmapped or ambiguous are counted per report
//! ([`Report::unmapped`], [`Report::ambiguous`]). With no known files at all,
//! paths under the root and relative paths are accepted as they are.
//!
//! # Staleness and line drift
//!
//! Coverage was measured at some earlier commit, so line numbers drift as
//! the file is edited after that. This crate does **not** try to correct
//! for that: a line range query answers in the line numbers of the report.
//! Instead it exposes when the report was measured ([`Report::measured_at`]:
//! the newest SimpleCov suite `timestamp`, else the file's mtime) so callers
//! can say "coverage from 3 days ago, before HEAD" ([`Coverage::freshness`])
//! rather than present it as current. Stale coverage is labelled, not hidden.
//!
//! # Unknown is not zero
//!
//! [`Coverage::file`] returns `None` for a file that is in no report:
//! coverage is **unknown**, never 0%. A file in a report with no covered
//! lines is a real 0%. A file with no coverable lines has no percentage.
//!
//! # Lifecycle
//!
//! [`Coverage::load`] reads every report once (whole file, streamed into the
//! bitsets without a JSON tree). A `Coverage` is immutable: a daemon keeps it
//! behind an `Arc`, polls [`Coverage::sources_changed`] (a few `stat`s and a
//! directory listing) and reloads in the background when it returns true.

mod formats;
mod paths;
#[cfg(test)]
mod tests;

pub use ripple_core::config::CoverageConfig;

use anyhow::{Result, bail};
use formats::LineBits;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Covered and coverable line counts of a file or of some of its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileCoverage {
    pub covered: u32,
    pub coverable: u32,
}

impl FileCoverage {
    /// `covered / coverable` in percent; `None` when nothing is coverable.
    pub fn percent(&self) -> Option<f64> {
        (self.coverable > 0).then(|| 100.0 * f64::from(self.covered) / f64::from(self.coverable))
    }

    pub fn uncovered(&self) -> u32 {
        self.coverable - self.covered
    }
}

/// One line of a file that is in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineState {
    /// Blank, comment, `end`, or beyond the measured length.
    NotCoverable,
    Covered,
    Uncovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    SimpleCov,
    Lcov,
    Istanbul,
}

impl Format {
    /// By file name: `*resultset*.json` SimpleCov, other `*.json` Istanbul,
    /// anything else LCOV.
    pub fn detect(path: &Path) -> Format {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.ends_with(".json") {
            if name.contains("resultset") {
                Format::SimpleCov
            } else {
                Format::Istanbul
            }
        } else {
            Format::Lcov
        }
    }
}

/// A report file that was read (or failed to be).
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub path: PathBuf,
    pub format: Format,
    /// Unix seconds.
    pub mtime: i64,
    pub size: u64,
    /// When the coverage was measured, unix seconds: the newest SimpleCov
    /// suite `timestamp`, else `mtime`.
    pub measured_at: i64,
    /// Distinct file entries in the report.
    pub entries: u32,
    pub mapped: u32,
    /// Entries that match no repo file (includes `ambiguous`).
    pub unmapped: u32,
    /// Entries that match several repo files equally well.
    pub ambiguous: u32,
    /// Entries or records skipped as malformed.
    pub malformed: u32,
    /// The report could not be read or parsed at all; it contributes nothing.
    pub error: Option<String>,
}

/// How current coverage data is. Stale data should be labelled, not hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// Unix seconds.
    pub measured_at: i64,
    pub age_secs: i64,
    /// Measured before the HEAD commit's committer time: the lines may have moved.
    pub older_than_head: bool,
    /// Older than `coverage.max_age_days`.
    pub older_than_max_age: bool,
}

impl Freshness {
    pub fn is_stale(&self) -> bool {
        self.older_than_head || self.older_than_max_age
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    nanos: u128,
    size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileRec {
    name_off: u32,
    name_len: u32,
    bits_off: u32,
    words: u32,
    covered: u32,
    coverable: u32,
    /// Index into `reports` of the newest-measured report with this file.
    report: u32,
}

/// Line coverage of a repository, merged from all reports. Immutable.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    cfg: CoverageConfig,
    names: String,
    files: Vec<FileRec>,
    /// Per file: `words` coverable words followed by `words` covered words.
    bits: Vec<u64>,
    reports: Vec<Report>,
    /// Every report file found, for [`Coverage::sources_changed`].
    sources: Vec<(PathBuf, Stamp)>,
    unmapped_sample: Vec<String>,
}

const UNMAPPED_SAMPLE: usize = 20;

impl Coverage {
    /// No coverage data.
    pub fn empty(cfg: &CoverageConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            names: String::new(),
            files: Vec::new(),
            bits: Vec::new(),
            reports: Vec::new(),
            sources: Vec::new(),
            unmapped_sample: Vec::new(),
        }
    }

    /// Reads all reports `cfg` points at (auto-detected in `coverage/` when
    /// `cfg.paths` is empty). `known_files` are the repo-relative paths of
    /// the repository's files, used to map report paths (see the crate docs);
    /// pass none to accept paths under `root` as they are.
    ///
    /// A report that cannot be parsed is skipped and listed with its
    /// [`Report::error`]; this fails only when reports exist and none of them
    /// could be read. No reports, or `enabled: false`, is an empty `Coverage`.
    pub fn load<'k>(
        root: &Path,
        cfg: &CoverageConfig,
        known_files: impl IntoIterator<Item = &'k str>,
    ) -> Result<Coverage> {
        let mut cov = Coverage::empty(cfg);
        if !cfg.enabled {
            return Ok(cov);
        }
        cov.sources = stamp_all(&discover(root, cfg));
        if cov.sources.is_empty() {
            return Ok(cov);
        }
        let mapper = paths::Mapper::new(root, known_files);
        let mut merged: HashMap<String, (LineBits, u32)> = HashMap::new();
        for (path, stamp) in &cov.sources {
            let format = Format::detect(path);
            let mtime = (stamp.nanos / 1_000_000_000) as i64;
            let mut rep = Report {
                path: path.clone(),
                format,
                mtime,
                size: stamp.size,
                measured_at: mtime,
                entries: 0,
                mapped: 0,
                unmapped: 0,
                ambiguous: 0,
                malformed: 0,
                error: None,
            };
            let parsed = std::fs::read(path)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| match format {
                    Format::SimpleCov => formats::parse_simplecov(&bytes),
                    Format::Lcov => formats::parse_lcov(&bytes),
                    Format::Istanbul => formats::parse_istanbul(&bytes),
                });
            let parsed = match parsed {
                Ok(p) => p,
                Err(e) => {
                    rep.error = Some(format!("{e:#}"));
                    cov.reports.push(rep);
                    continue;
                }
            };
            if let Some(t) = parsed.timestamp {
                rep.measured_at = t;
            }
            rep.malformed = parsed.malformed;
            rep.entries = parsed.entries.len() as u32;
            let idx = cov.reports.len() as u32;
            let base = project_dir(root, path);
            let raws: Vec<&str> = parsed.entries.keys().map(String::as_str).collect();
            let outcomes = mapper.resolve(&raws, &base);
            let mut mapped_here: Vec<(String, &str)> = Vec::new();
            for (raw, o) in raws.iter().zip(outcomes) {
                match o {
                    paths::Outcome::Mapped(p) => mapped_here.push((p, raw)),
                    paths::Outcome::Unmapped | paths::Outcome::Ambiguous => {
                        rep.unmapped += 1;
                        rep.ambiguous += u32::from(o == paths::Outcome::Ambiguous);
                        if cov.unmapped_sample.len() < UNMAPPED_SAMPLE {
                            cov.unmapped_sample.push(raw.to_string());
                        }
                    }
                }
            }
            rep.mapped = mapped_here.len() as u32;
            for (p, raw) in mapped_here {
                let bits = &parsed.entries[raw];
                match merged.get_mut(&p) {
                    Some((b, r)) => {
                        b.or(bits);
                        if rep.measured_at >= cov.reports[*r as usize].measured_at {
                            *r = idx;
                        }
                    }
                    None => {
                        merged.insert(p, (bits.clone(), idx));
                    }
                }
            }
            cov.reports.push(rep);
        }
        if cov.reports.iter().all(|r| r.error.is_some()) {
            let errs: Vec<String> = cov
                .reports
                .iter()
                .map(|r| format!("{}: {}", r.path.display(), r.error.as_deref().unwrap_or("")))
                .collect();
            bail!("no coverage report could be read: {}", errs.join("; "));
        }
        cov.finish(merged);
        Ok(cov)
    }

    fn finish(&mut self, merged: HashMap<String, (LineBits, u32)>) {
        let mut all: Vec<(String, (LineBits, u32))> = merged.into_iter().collect();
        all.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let words: usize = all
            .iter()
            .map(|(_, (b, _))| b.cov.len().min(b.hit.len()) * 2)
            .sum();
        self.names
            .reserve_exact(all.iter().map(|(p, _)| p.len()).sum());
        self.bits.reserve_exact(words);
        self.files.reserve_exact(all.len());
        for (path, (mut b, report)) in all {
            b.fix();
            let name_off = self.names.len() as u32;
            self.names.push_str(&path);
            let bits_off = self.bits.len() as u32;
            let n = b.cov.len();
            self.bits.extend_from_slice(&b.cov);
            self.bits.extend_from_slice(&b.hit[..n]);
            let coverable = b.cov.iter().map(|w| w.count_ones()).sum();
            let covered = b.hit.iter().map(|w| w.count_ones()).sum();
            self.files.push(FileRec {
                name_off,
                name_len: path.len() as u32,
                bits_off,
                words: n as u32,
                covered,
                coverable,
                report,
            });
        }
    }

    fn name(&self, r: &FileRec) -> &str {
        &self.names[r.name_off as usize..(r.name_off + r.name_len) as usize]
    }

    fn rec(&self, path: &str) -> Option<&FileRec> {
        self.files
            .binary_search_by(|r| self.name(r).cmp(path))
            .ok()
            .map(|i| &self.files[i])
    }

    fn words(&self, r: &FileRec) -> (&[u64], &[u64]) {
        let (o, n) = (r.bits_off as usize, r.words as usize);
        (&self.bits[o..o + n], &self.bits[o + n..o + 2 * n])
    }

    /// Whole-file coverage. `None` = the file is in no report: UNKNOWN, not 0%.
    pub fn file(&self, path: &str) -> Option<FileCoverage> {
        self.rec(path).map(|r| FileCoverage {
            covered: r.covered,
            coverable: r.coverable,
        })
    }

    /// Coverage of the lines in `ranges` (1-based, inclusive, in the line
    /// numbers of the report; overlaps count once). `None` = file unknown.
    /// `Some` with `coverable == 0` = none of these lines is executable.
    pub fn lines(&self, path: &str, ranges: &[(u32, u32)]) -> Option<FileCoverage> {
        let r = self.rec(path)?;
        let (cov, hit) = self.words(r);
        let mut out = FileCoverage::default();
        let mut add = |s: u32, e: u32| {
            out.coverable += count_range(cov, s, e);
            out.covered += count_range(hit, s, e);
        };
        let sorted_disjoint = ranges.windows(2).all(|w| w[0].1 < w[1].0);
        if sorted_disjoint {
            for &(s, e) in ranges {
                add(s, e);
            }
        } else {
            let mut rs: Vec<(u32, u32)> = ranges.iter().copied().filter(|(s, e)| s <= e).collect();
            rs.sort_unstable();
            let mut cur: Option<(u32, u32)> = None;
            for (s, e) in rs {
                match &mut cur {
                    Some((_, ce)) if s <= ce.saturating_add(1) => *ce = (*ce).max(e),
                    _ => {
                        if let Some((cs, ce)) = cur {
                            add(cs, ce);
                        }
                        cur = Some((s, e));
                    }
                }
            }
            if let Some((cs, ce)) = cur {
                add(cs, ce);
            }
        }
        Some(out)
    }

    /// One line (1-based). `None` = file unknown.
    pub fn line(&self, path: &str, line: u32) -> Option<LineState> {
        let r = self.rec(path)?;
        let (cov, hit) = self.words(r);
        let Some(i) = line.checked_sub(1).map(|i| i as usize) else {
            return Some(LineState::NotCoverable);
        };
        let (w, b) = (i / 64, 1u64 << (i % 64));
        Some(
            match (
                cov.get(w).is_some_and(|x| x & b != 0),
                hit.get(w).is_some_and(|x| x & b != 0),
            ) {
                (false, _) => LineState::NotCoverable,
                (true, true) => LineState::Covered,
                (true, false) => LineState::Uncovered,
            },
        )
    }

    /// Uncovered coverable lines within `ranges` (1-based, inclusive), ascending.
    pub fn uncovered_lines(&self, path: &str, ranges: &[(u32, u32)]) -> Option<Vec<u32>> {
        let r = self.rec(path)?;
        let (cov, hit) = self.words(r);
        let mut out = Vec::new();
        for (w, (&c, &h)) in cov.iter().zip(hit).enumerate() {
            let mut m = c & !h;
            while m != 0 {
                let line = (w * 64) as u32 + m.trailing_zeros() + 1;
                if ranges.iter().any(|&(s, e)| s <= line && line <= e) {
                    out.push(line);
                }
                m &= m - 1;
            }
        }
        Some(out)
    }

    /// Files with coverage, sorted by path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, FileCoverage)> {
        self.files.iter().map(|r| {
            (
                self.name(r),
                FileCoverage {
                    covered: r.covered,
                    coverable: r.coverable,
                },
            )
        })
    }

    /// Sum over all files.
    pub fn totals(&self) -> FileCoverage {
        self.files
            .iter()
            .fold(FileCoverage::default(), |a, r| FileCoverage {
                covered: a.covered + r.covered,
                coverable: a.coverable + r.coverable,
            })
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Every report file found, including ones that failed ([`Report::error`]).
    pub fn reports(&self) -> &[Report] {
        &self.reports
    }

    /// Report entries that could not be mapped to a repo file, over all reports.
    pub fn unmapped(&self) -> u32 {
        self.reports.iter().map(|r| r.unmapped).sum()
    }

    /// Up to 20 unmapped report paths as written in the report, for diagnostics.
    pub fn unmapped_sample(&self) -> &[String] {
        &self.unmapped_sample
    }

    pub fn config(&self) -> &CoverageConfig {
        &self.cfg
    }

    fn ok_reports(&self) -> impl Iterator<Item = &Report> {
        self.reports.iter().filter(|r| r.error.is_none())
    }

    /// Newest mtime of a report that was read, unix seconds.
    pub fn newest_mtime(&self) -> Option<i64> {
        self.ok_reports().map(|r| r.mtime).max()
    }

    /// When the data was measured, unix seconds: the *oldest* `measured_at`
    /// of the reports read (conservative: some of the data is this old).
    pub fn measured_at(&self) -> Option<i64> {
        self.ok_reports().map(|r| r.measured_at).min()
    }

    /// When the coverage of `path` was measured: the newest report with it.
    pub fn file_measured_at(&self, path: &str) -> Option<i64> {
        self.rec(path)
            .map(|r| self.reports[r.report as usize].measured_at)
    }

    /// Freshness of the whole data set (`path = None`, using
    /// [`Coverage::measured_at`]) or of one file. `head_commit_time` is the
    /// HEAD commit's committer time and `now` the current time, unix seconds.
    /// `None` = no data (or the file is unknown).
    pub fn freshness(
        &self,
        path: Option<&str>,
        head_commit_time: Option<i64>,
        now: i64,
    ) -> Option<Freshness> {
        let measured_at = match path {
            Some(p) => self.file_measured_at(p)?,
            None => self.measured_at()?,
        };
        Some(Freshness {
            measured_at,
            age_secs: (now - measured_at).max(0),
            older_than_head: head_commit_time.is_some_and(|h| measured_at < h),
            older_than_max_age: self
                .cfg
                .max_age_days
                .is_some_and(|d| now - measured_at > i64::from(d) * 86_400),
        })
    }

    /// True when a report file appeared, disappeared, or changed (mtime or
    /// size) since this was loaded: reload. A few `stat`s; no file is read.
    pub fn sources_changed(&self, root: &Path) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        stamp_all(&discover(root, &self.cfg)) != self.sources
    }

    /// Heap memory held, in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.names.capacity()
            + self.files.capacity() * std::mem::size_of::<FileRec>()
            + self.bits.capacity() * 8
            + self.reports.capacity() * std::mem::size_of::<Report>()
            + self
                .reports
                .iter()
                .map(|r| r.path.capacity() + r.error.as_ref().map_or(0, String::capacity))
                .sum::<usize>()
            + self.sources.capacity() * std::mem::size_of::<(PathBuf, Stamp)>()
            + self
                .sources
                .iter()
                .map(|(p, _)| p.capacity())
                .sum::<usize>()
            + self.unmapped_sample.capacity() * std::mem::size_of::<String>()
            + self
                .unmapped_sample
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + self.cfg.paths.iter().map(String::capacity).sum::<usize>()
    }
}

/// Set bits in positions `[s-1, e-1]` (lines `s..=e`).
fn count_range(words: &[u64], s: u32, e: u32) -> u32 {
    let s = s.max(1);
    if s > e {
        return 0;
    }
    let (lo, hi) = ((s - 1) as usize, (e - 1) as usize);
    let max = words.len() * 64;
    if lo >= max {
        return 0;
    }
    let hi = hi.min(max - 1);
    let (lw, hw) = (lo / 64, hi / 64);
    let lmask = !0u64 << (lo % 64);
    let hmask = !0u64 >> (63 - hi % 64);
    if lw == hw {
        return (words[lw] & lmask & hmask).count_ones();
    }
    let mut n = (words[lw] & lmask).count_ones() + (words[hw] & hmask).count_ones();
    for w in &words[lw + 1..hw] {
        n += w.count_ones();
    }
    n
}

/// Repo-relative directory a report's relative paths are relative to: the
/// parent of the nearest enclosing `coverage` directory, else the report's
/// own directory.
fn project_dir(root: &Path, report: &Path) -> String {
    let rel = report.strip_prefix(root).unwrap_or(report);
    let dir = rel.parent().unwrap_or(Path::new(""));
    let comps: Vec<String> = dir
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let cut = comps
        .iter()
        .rposition(|c| c == "coverage")
        .unwrap_or(comps.len());
    comps[..cut].join("/")
}

const DEFAULT_NAMES: [&str; 2] = [".resultset.json", "lcov.info"];

/// Report files `cfg` points at, sorted, deduplicated.
fn discover(root: &Path, cfg: &CoverageConfig) -> Vec<PathBuf> {
    let default = ["coverage".to_string()];
    let entries: &[String] = if cfg.paths.is_empty() {
        &default
    } else {
        &cfg.paths
    };
    let mut out = Vec::new();
    for e in entries {
        let full = root.join(e);
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.contains('*') {
            glob_dir(full.parent().unwrap_or(root), &name, &mut out);
        } else if full.is_dir() {
            for n in DEFAULT_NAMES {
                let p = full.join(n);
                if p.is_file() {
                    out.push(p);
                }
            }
            glob_dir(&full.join("lcov"), "*.lcov", &mut out);
            let istanbul = full.join("coverage-final.json");
            if !full.join("lcov.info").is_file() && istanbul.is_file() {
                out.push(istanbul);
            }
        } else if full.is_file() {
            out.push(full);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn glob_dir(dir: &Path, pattern: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let n = e.file_name();
        if wildcard(pattern, &n.to_string_lossy()) && e.path().is_file() {
            out.push(e.path());
        }
    }
}

/// `*` matches any run of characters; everything else is literal.
fn wildcard(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

fn stamp_all(paths: &[PathBuf]) -> Vec<(PathBuf, Stamp)> {
    paths
        .iter()
        .filter_map(|p| {
            let m = std::fs::metadata(p).ok()?;
            let nanos = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            Some((
                p.clone(),
                Stamp {
                    nanos,
                    size: m.len(),
                },
            ))
        })
        .collect()
}

/// Current time, unix seconds (for [`Coverage::freshness`]).
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
