//! `ripple-memprof ROOT`: live-heap attribution of a cold index, by owner.
//!
//! Built only with `--features mem-profile`. Runs the same stages as
//! `Engine::full_build` (list files, extractors, linkers, graph, policy
//! index), the daemon's git tracker and file watcher, under a counting
//! global allocator; reports the live bytes and peak after each stage, then
//! frees the pieces one by one and reports what each freed.
//!
//! Byte counts are the allocator's usable sizes (`malloc_size` /
//! `malloc_usable_size`), comparable to `vmmap`'s "allocated".

use ripple_core::{Extractor, Facts, Graph, Linker, Workspace};
use ripple_policy::{Policy, PolicyIndex};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::time::Instant;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
static BLOCKS: AtomicIsize = AtomicIsize::new(0);

unsafe extern "C" {
    #[cfg(target_os = "macos")]
    fn malloc_size(ptr: *const std::ffi::c_void) -> usize;
    #[cfg(not(target_os = "macos"))]
    fn malloc_usable_size(ptr: *mut std::ffi::c_void) -> usize;
}

fn usable(p: *mut u8) -> isize {
    #[cfg(target_os = "macos")]
    unsafe {
        malloc_size(p as *const _) as isize
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        malloc_usable_size(p as *mut _) as isize
    }
}

struct Counting;

fn add(n: isize) {
    let now = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            add(usable(p));
            BLOCKS.fetch_add(1, Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            add(usable(p));
            BLOCKS.fetch_add(1, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(usable(p), Relaxed);
        BLOCKS.fetch_sub(1, Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let old = usable(p);
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            LIVE.fetch_sub(old, Relaxed);
            add(usable(q));
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> isize {
    LIVE.load(Relaxed)
}

fn mb(b: isize) -> String {
    format!("{:8.1} MB", b as f64 / 1048576.0)
}

struct Phase {
    t: Instant,
    live: isize,
}

impl Phase {
    fn start() -> Self {
        PEAK.store(live(), Relaxed);
        Self { t: Instant::now(), live: live() }
    }
    fn end(self, label: &str) {
        println!(
            "phase {label:<28} retained {}  peak-during {} (+{})  live {}  blocks {}  {:7.0} ms",
            mb(live() - self.live),
            mb(PEAK.load(Relaxed)),
            mb(PEAK.load(Relaxed) - self.live),
            mb(live()),
            BLOCKS.load(Relaxed),
            self.t.elapsed().as_secs_f64() * 1000.0
        );
    }
}

fn freed(label: &str, f: impl FnOnce()) -> isize {
    let before = live();
    f();
    let d = before - live();
    println!("free  {label:<50} {}", mb(d));
    d
}

fn main() -> anyhow::Result<()> {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: ripple-memprof ROOT"));
    let root = root.canonicalize()?;
    let start = live();
    let all = Phase::start();

    let p = Phase::start();
    let policy = Policy::load(&root).unwrap_or_else(|_| Policy::compile(ripple_core::Config::default()).expect("defaults"));
    let (files, _) = ripple_cli::files::list_files(&root, &policy)?;
    let mut ws = Workspace { root: root.clone(), files, config: policy.config.clone() };
    let members: std::collections::HashSet<String> = ws.files.iter().cloned().collect();
    p.end("list_files+policy");

    let p = Phase::start();
    let mut ruby = ripple_ruby::RubyExtractor::new();
    let mut ruby_out = ruby.build(&ws)?;
    p.end("ruby.build");

    let p = Phase::start();
    let mut js = ripple_js::JsExtractor::new();
    let mut js_out = js.build(&ws)?;
    p.end("js.build");

    let p = Phase::start();
    // As `Engine::run_linkers`: merged facts (a temporary), scopes kept.
    let mut facts = Facts::default();
    for o in [&ruby_out, &js_out] {
        facts.merge(o.facts.clone());
    }
    let mut scopes = std::mem::take(&mut facts.scopes);
    for r in scopes.values_mut() {
        r.sort_by_key(|r| (r.start, r.end));
    }
    scopes.retain(|_, v| !v.is_empty());
    let engine_scopes = std::sync::Arc::new(scopes.clone());
    facts.scopes = scopes;
    let mut boundary = ripple_boundary::BoundaryLinker::new();
    let mut b_out = boundary.link(&ws, &facts, None)?;
    drop(facts);
    p.end("boundary.link");

    let p = Phase::start();
    let edges = ruby_out.edges.iter().chain(&js_out.edges).chain(&b_out.edges);
    let mut graph = Graph::build(edges, ws.files.iter().map(String::as_str));
    if let Some(m) = ruby_out.facts.mentions.as_ref() {
        graph.set_mentions(m);
    }
    p.end("graph.build");
    let p = Phase::start();
    let index = PolicyIndex::build(&policy, &graph);
    p.end("policy_index.build");
    all.end("TOTAL cold build");
    let after_build = live();

    let p = Phase::start();
    let git = ripple_cli::gitd::GitTracker::new(&root);
    git.prewarm();
    p.end("git tracker prewarm");

    let p = Phase::start();
    let watcher = {
        use notify::{RecursiveMode, Watcher};
        let mut w = notify::recommended_watcher(|_res: notify::Result<notify::Event>| {})?;
        w.watch(&root, RecursiveMode::Recursive)?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
        w
    };
    p.end("fs watcher");
    let total = live() - start;
    println!("live after everything: {}", mb(total));
    println!("counts: nodes {} edges {} ruby_edges {} js_edges {} boundary_edges {}", graph.node_count(), graph.edge_count(), ruby_out.edges.len(), js_out.edges.len(), b_out.edges.len());
    if let Some(path) = std::env::var_os("MEMPROF_DUMP") {
        // Everything the index publishes, for before/after equality checks.
        use std::fmt::Write as _;
        let mut s = String::new();
        for e in 0..graph.edge_count() as u32 {
            let (f, t, d) = graph.edge(e);
            let _ = writeln!(s, "E {} {} {:?} {:?} {} {:?} {:?} {}", graph.path(f), graph.path(t), d.kind, d.confidence, d.line, graph.edge_detail(e), graph.edge_scopes(e), graph.edge_inherits(e));
        }
        for n in 0..graph.node_count() as u32 {
            let _ = writeln!(s, "M {} {:?}", graph.path(n), graph.mentions(n));
        }
        for (f, r) in engine_scopes.iter() {
            let _ = writeln!(s, "S {f} {r:?}");
        }
        for o in [&ruby_out, &js_out, &b_out] {
            for w in &o.warnings {
                let _ = writeln!(s, "W {w:?}");
            }
        }
        std::fs::write(path, s)?;
    }
    if std::env::var_os("MEMPROF_PAUSE").is_some() {
        println!("pid {} paused; press enter", std::process::id());
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
    }

    // Tear-down attribution, roughly in reverse order of ownership.
    let mut sum = 0;
    sum += freed("watcher", || drop(watcher));
    sum += freed("git tracker", || drop(git));
    sum += freed("snapshot.policy_index", || drop(index));
    sum += freed("snapshot.graph (mention Arcs shared)", || drop(graph));
    sum += freed("snapshot/engine.scopes (Arc<BTreeMap>)", || drop(engine_scopes));
    sum += freed("engine.files HashSet", || drop(members));
    sum += freed("engine.link_outputs[boundary].edges", || b_out.edges = Vec::new());
    sum += freed("engine.link_outputs[boundary].rest", || drop(b_out));
    sum += freed("boundary linker state", || drop(boundary));
    sum += freed("engine.outputs[js].edges", || js_out.edges = Vec::new());
    sum += freed("engine.outputs[js].facts(call sites)+rest", || drop(js_out));
    for (k, v) in ripple_js::memprof::breakdown(&mut js, &live) {
        if k.starts_with("measured.") {
            sum += v;
            println!("free  {:<50} {}", &k[9..], mb(v));
        } else {
            println!("info  {k:<50} {v}");
        }
    }
    sum += freed("js extractor rest (pool)", || drop(js));
    sum += freed("engine.outputs[ruby].edges", || ruby_out.edges = Vec::new());
    sum += freed("engine.outputs[ruby].facts.scopes", || ruby_out.facts.scopes = Default::default());
    sum += freed("engine.outputs[ruby].facts.mentions (table; Arcs shared)", || ruby_out.facts.mentions = None);
    sum += freed("engine.outputs[ruby].warnings+stats", || drop(ruby_out));
    for (k, v) in ripple_ruby::memprof::breakdown(&mut ruby, &live) {
        if let Some(l) = k.strip_prefix("measured.") {
            sum += v;
            println!("free  {l:<50} {}", mb(v));
        } else if k.starts_with("est.") {
            println!("est   {:<50} {}", &k[4..], mb(v));
        } else {
            println!("info  {k:<50} {v}");
        }
    }
    sum += freed("ruby extractor rest", || drop(ruby));
    sum += freed("ws.files", || ws.files = Vec::new());
    sum += freed("ws + policy", || {
        drop(ws);
        drop(policy)
    });
    println!("sum of frees {}  (live after build+daemon parts {}; remaining live {})", mb(sum), mb(total), mb(live() - start));
    let _ = after_build;
    Ok(())
}
