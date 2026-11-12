//! `cargo run --release -p ripple-history --example bench -- <repo> <sidecar> [max_commits] [behind]`
//!
//! Measures a cold build at HEAD, save, load, an incremental update from
//! `HEAD~behind` (first parent) to HEAD, and query latency. Writes only the
//! sidecar path given (keep it outside the repository).

use ripple_history::{History, Options, PartnerQuery};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo = PathBuf::from(&args[0]);
    let sidecar = PathBuf::from(&args[1]);
    let max_commits: usize = args.get(2).map_or(Ok(5000), |s| s.parse())?;
    let behind: usize = args.get(3).map_or(Ok(50), |s| s.parse())?;
    let opts = Options { max_commits, ..Default::default() };
    let no = AtomicBool::new(false);

    let t = Instant::now();
    let h = History::build(&repo, "HEAD", &opts, &no)?;
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    h.save(&sidecar)?;
    let save_ms = t.elapsed().as_secs_f64() * 1e3;
    let size = std::fs::metadata(&sidecar)?.len();
    let mut loads = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let l = History::load(&sidecar)?;
        loads.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(l, h);
    }
    loads.sort_by(f64::total_cmp);
    println!(
        "build {build_ms:.0} ms: {} commits ({} non-sweep), {} files, {} partner entries; heap {:.1} MB; save {save_ms:.1} ms; file {:.2} MB; load p50 {:.1} ms (min {:.1})",
        h.commit_count(),
        h.pair_commit_count(),
        h.file_count(),
        h.pair_count(),
        h.heap_bytes() as f64 / 1e6,
        size as f64 / 1e6,
        loads[2],
        loads[0]
    );

    // Incremental update from HEAD~behind.
    let old_rev = format!("HEAD~{behind}");
    let t = Instant::now();
    let old = History::build(&repo, &old_rev, &opts, &no)?;
    let old_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let (upd, stats) = old.update(&repo, "HEAD", &opts, &no)?;
    let upd_ms = t.elapsed().as_secs_f64() * 1e3;
    println!("update {old_rev} -> HEAD: {upd_ms:.0} ms ({stats:?}); equals full build: {}; (build at {old_rev}: {old_ms:.0} ms)", upd == h);
    let t = Instant::now();
    let (_, stats) = h.update(&repo, "HEAD", &opts, &no)?;
    println!("update HEAD -> HEAD (no-op): {:.1} ms {stats:?}", t.elapsed().as_secs_f64() * 1e3);

    // Query latency: every 7th file with history, singles and 5-file seed sets.
    let files: Vec<String> = (0..h.file_count()).step_by(7).filter_map(|i| h.path_at(i)).map(str::to_string).collect();
    let q = PartnerQuery::default();
    let mut lat = Vec::new();
    let mut nonempty = 0usize;
    for f in &files {
        let t = Instant::now();
        let p = h.partners_with(std::slice::from_ref(f), &q, |_| false);
        lat.push(t.elapsed().as_nanos() as f64 / 1e3);
        nonempty += usize::from(!p.is_empty());
    }
    let mut lat5 = Vec::new();
    for w in files.chunks(5) {
        let t = Instant::now();
        let _ = h.partners_with(w, &q, |_| false);
        lat5.push(t.elapsed().as_nanos() as f64 / 1e3);
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
    };
    println!(
        "query (default filters, limit 5): 1 seed p50 {:.1} µs p99 {:.1} µs max {:.1} µs over {}; 5 seeds p50 {:.1} µs p99 {:.1} µs; non-empty for {:.0}% of single files",
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.99),
        pct(&mut lat, 1.0),
        lat.len(),
        pct(&mut lat5, 0.5),
        pct(&mut lat5, 0.99),
        100.0 * nonempty as f64 / files.len().max(1) as f64
    );
    Ok(())
}
