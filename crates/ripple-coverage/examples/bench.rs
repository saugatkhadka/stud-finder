//! `cargo run --release -p ripple-coverage --example bench -- <repo> <out_dir> [js_files]`
//!
//! Generates synthetic reports for the files of `<repo>` (read-only: only
//! `git ls-files` and file lengths are read) under `<out_dir>/coverage/`:
//! a SimpleCov resultset for every `*.rb` file (two suites, the second on
//! every 10th file; paths in a fake CI checkout `/builds/org/repo`) and an
//! LCOV file for the first `js_files` (default 5000) `*.js`/`*.vue`/`*.ts`
//! files. Then measures load, heap and lookup latency with `<out_dir>` as the
//! root and the repo's file list as the known files.

use ripple_coverage::{Coverage, CoverageConfig};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pct(&mut self) -> u64 {
        self.next() % 100
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let repo = PathBuf::from(&args[0]);
    let out = PathBuf::from(&args[1]);
    let js_n: usize = args.get(2).map_or(Ok(5000), |s| s.parse())?;
    let ls = std::process::Command::new("git").arg("-C").arg(&repo).args(["ls-files", "-z"]).output()?;
    let known: Vec<String> = ls.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let lines_of = |p: &str| std::fs::read(repo.join(p)).map(|b| b.iter().filter(|&&c| c == b'\n').count().max(1)).unwrap_or(1);
    let rb: Vec<&String> = known.iter().filter(|p| p.ends_with(".rb")).collect();
    let js: Vec<&String> = known.iter().filter(|p| p.ends_with(".js") || p.ends_with(".vue") || p.ends_with(".ts")).take(js_n).collect();

    let t = Instant::now();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    std::fs::create_dir_all(out.join("coverage"))?;
    let mut total_lines = 0usize;
    let mut rs = String::from("{");
    for (si, (suite, step)) in [("RSpec", 1usize), ("Minitest", 10)].into_iter().enumerate() {
        if si > 0 {
            rs.push(',');
        }
        write!(rs, "\"{suite}\":{{\"coverage\":{{")?;
        for (i, p) in rb.iter().enumerate().filter(|(i, _)| i % step == 0) {
            if i > 0 {
                rs.push(',');
            }
            let n = lines_of(p);
            total_lines += n;
            write!(rs, "\"/builds/org/repo/{p}\":{{\"lines\":[")?;
            for l in 0..n {
                if l > 0 {
                    rs.push(',');
                }
                match rng.pct() {
                    0..30 => rs.push_str("null"),
                    30..55 => rs.push('0'),
                    _ => write!(rs, "{}", rng.next() % 50 + 1)?,
                }
            }
            rs.push_str("],\"branches\":{}}");
        }
        write!(rs, "}},\"timestamp\":{}}}", 1_760_000_000 + si)?;
    }
    rs.push('}');
    std::fs::write(out.join("coverage/.resultset.json"), &rs)?;
    let mut lcov = String::new();
    for p in &js {
        write!(lcov, "TN:\nSF:/builds/org/repo/{p}\n")?;
        let n = lines_of(p);
        total_lines += n;
        for l in 1..=n {
            if rng.pct() < 60 {
                writeln!(lcov, "DA:{l},{}", if rng.pct() < 30 { 0 } else { rng.next() % 9 + 1 })?;
            }
        }
        lcov.push_str("end_of_record\n");
    }
    std::fs::write(out.join("coverage/lcov.info"), &lcov)?;
    println!(
        "generated in {:.1}s: resultset {} rb files ({:.1} MB), lcov {} js files ({:.1} MB), {} source lines; {} known files",
        t.elapsed().as_secs_f64(),
        rb.len(),
        rs.len() as f64 / 1e6,
        js.len(),
        lcov.len() as f64 / 1e6,
        total_lines,
        known.len()
    );
    drop((rs, lcov));

    let cfg = CoverageConfig::default();
    let mut loads = Vec::new();
    let mut cov = None;
    for _ in 0..5 {
        let t = Instant::now();
        let c = Coverage::load(&out, &cfg, known.iter().map(String::as_str))?;
        loads.push(t.elapsed().as_secs_f64() * 1e3);
        cov = Some(c);
    }
    let cov = cov.unwrap();
    loads.sort_by(f64::total_cmp);
    for r in cov.reports() {
        println!("  {:?} {}: entries {} mapped {} unmapped {} malformed {} err {:?}", r.format, r.path.display(), r.entries, r.mapped, r.unmapped, r.malformed, r.error);
    }
    // Load time per format.
    for (name, paths) in [("simplecov only", vec!["coverage/.resultset.json"]), ("lcov only", vec!["coverage/lcov.info"])] {
        let c = CoverageConfig { paths: paths.into_iter().map(String::from).collect(), ..Default::default() };
        let mut v = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            Coverage::load(&out, &c, known.iter().map(String::as_str))?;
            v.push(t.elapsed().as_secs_f64() * 1e3);
        }
        v.sort_by(f64::total_cmp);
        println!("  load {name}: p50 {:.1} ms (min {:.1})", v[2], v[0]);
    }
    let rss = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output()?;
    println!(
        "load p50 {:.1} ms (min {:.1}, max {:.1}); {} files; heap {:.2} MB; process RSS {:.1} MB",
        loads[2],
        loads[0],
        loads[4],
        cov.file_count(),
        cov.heap_bytes() as f64 / 1e6,
        String::from_utf8_lossy(&rss.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
    );

    // Lookups.
    let paths: Vec<&str> = cov.iter().map(|(p, _)| p).collect();
    let mut rng = Rng(42);
    let q: Vec<&str> = (0..200_000).map(|_| paths[(rng.next() % paths.len() as u64) as usize]).collect();
    let t = Instant::now();
    let mut acc = 0u64;
    for p in &q {
        acc += u64::from(cov.file(p).unwrap().covered);
    }
    let file_ns = t.elapsed().as_nanos() as f64 / q.len() as f64;
    let unknown: Vec<&str> = known.iter().filter(|p| cov.file(p).is_none()).take(20_000).map(String::as_str).collect();
    let t = Instant::now();
    for p in &unknown {
        acc += u64::from(cov.file(p).is_none());
    }
    let unk_ns = t.elapsed().as_nanos() as f64 / unknown.len() as f64;
    let t = Instant::now();
    for (i, p) in q.iter().enumerate() {
        let s = (i % 200) as u32 + 1;
        acc += u64::from(cov.lines(p, &[(s, s + 20), (s + 40, s + 45)]).unwrap().coverable);
    }
    let lines_ns = t.elapsed().as_nanos() as f64 / q.len() as f64;
    let t = Instant::now();
    for p in &q[..20_000] {
        acc += u64::from(cov.lines(p, &[(1, 100_000)]).unwrap().coverable);
    }
    let whole_ns = t.elapsed().as_nanos() as f64 / 20_000.0;
    let t = Instant::now();
    let mut changed = false;
    for _ in 0..1000 {
        changed |= cov.sources_changed(&out);
    }
    let sc_us = t.elapsed().as_secs_f64() * 1e6 / 1000.0;
    println!(
        "file() {file_ns:.0} ns; file() unknown {unk_ns:.0} ns; lines(2 ranges) {lines_ns:.0} ns; lines(whole file) {whole_ns:.0} ns; sources_changed {sc_us:.1} us (changed={changed}); checksum {acc}"
    );
    let tot = cov.totals();
    println!("totals {}/{} = {:.1}%", tot.covered, tot.coverable, tot.percent().unwrap_or(0.0));
    Ok(())
}
