//! Command-line interface.

use crate::client::{Backend, Client, Reply, spawn_daemon, wait_for_daemon};
use crate::engine::Pipeline;
use crate::files;
use crate::git::{self, DiffSpec};
use crate::paths::DaemonPaths;
use crate::protocol::{DiffRequest, Format, ImpactParams, PROTOCOL};
use crate::server::{self, ServeOptions};
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use ripple_core::{Confidence, Risk};
use ripple_policy::{Report, render};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "ripple", version, about = "Change impact analysis for Rails + React monorepos", propagate_version = true)]
pub struct Cli {
    /// Repository root (default: the enclosing git work tree).
    #[arg(long, global = true, env = "RIPPLE_ROOT")]
    root: Option<PathBuf>,
    /// Index in this process instead of using (or starting) the daemon.
    #[arg(long, global = true)]
    no_daemon: bool,
    /// Use this config file instead of `<root>/ripple.yml` (a separate daemon serves it).
    #[arg(long, global = true, env = "RIPPLE_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what a change affects (default: working tree vs HEAD, including untracked files).
    Impact(ImpactArgs),
    /// CI entry point: impact of the branch vs a base ref; exits 1 at or above `--fail-on`.
    Check(CheckArgs),
    /// What depends on FILE (or some of its methods): the `impact` report of FILE as if it changed.
    Dependents(DependentsArgs),
    /// Files FILE depends on.
    Dependencies(NeighbourArgs),
    /// Print the JSON Schema of the report (`impact`, `dependents`, `check --format json`).
    Schema,
    /// Shortest dependency path between two files (FROM depends on TO, or the reverse).
    Why {
        from: String,
        to: String,
        /// Only paths through this method / controller action of TO (repeatable).
        #[arg(long = "scope", value_name = "NAME")]
        scopes: Vec<String>,
        #[arg(long, value_enum, default_value_t = OutFormat::Text)]
        format: OutFormat,
    },
    /// How ripple.yml classifies a file (areas, test, global, language).
    Areas {
        file: String,
        #[arg(long, value_enum, default_value_t = OutFormat::Text)]
        format: OutFormat,
    },
    /// One file's profile: metrics, its most complex methods, churn,
    /// coverage, dependents, direct tests, co-change partners, hotspot rank.
    File {
        file: String,
        #[arg(long, value_enum, default_value_t = OutFormat::Text)]
        format: OutFormat,
        /// Answer at once from the current index, even while git history,
        /// coverage or the ranking are still loading (default: wait up to 5 s for them).
        #[arg(long)]
        no_wait: bool,
    },
    /// The riskiest non-test source files: recent churn, complexity,
    /// missing coverage and dependents, as percentile ranks.
    Rank(RankArgs),
    /// Index statistics.
    Stats {
        #[arg(long, value_enum, default_value_t = OutFormat::Text)]
        format: OutFormat,
    },
    /// Write a commented starter ripple.yml.
    Init {
        /// Overwrite an existing ripple.yml.
        #[arg(long)]
        force: bool,
    },
    /// Validate ripple.yml.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Manage the background daemon for this repository.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// The git-history cache (churn, co-change) at `<git common dir>/ripple/history[-<worktree>].bin`.
    History {
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    /// Run the daemon (in the background unless --foreground).
    Serve {
        #[arg(long)]
        foreground: bool,
    },
    /// Benchmarks and synthetic data (development).
    #[command(hide = true)]
    Bench(crate::bench::BenchArgs),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Parse and validate ripple.yml and show what its globs match.
    Check,
}

#[derive(Subcommand)]
enum HistoryCmd {
    /// Build or update the cache to HEAD in the foreground.
    Build {
        /// Build at this commit instead of HEAD (benchmarks, CI caches).
        #[arg(long, value_name = "REV", hide = true)]
        at: Option<String>,
    },
    /// Show the running daemon's history state, else the cache's.
    Status,
}

#[derive(Subcommand)]
enum DaemonCmd {
    Status,
    Stop,
    Start,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OutFormat {
    Text,
    Json,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ConfArg {
    Exact,
    Inferred,
    Heuristic,
}

impl From<ConfArg> for Confidence {
    fn from(c: ConfArg) -> Self {
        match c {
            ConfArg::Exact => Confidence::Exact,
            ConfArg::Inferred => Confidence::Inferred,
            ConfArg::Heuristic => Confidence::Heuristic,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum RiskArg {
    Low,
    Medium,
    High,
    Critical,
    /// Never fail.
    Never,
}

#[derive(Args)]
struct DiffArgs {
    /// Compare against merge-base(REF, HEAD), including uncommitted and untracked files.
    #[arg(long, value_name = "REF", conflicts_with_all = ["staged", "files_csv", "files"])]
    base: Option<String>,
    /// Only staged changes (index vs HEAD).
    #[arg(long, conflicts_with_all = ["files_csv", "files"])]
    staged: bool,
    /// Explicit changed files, comma separated.
    #[arg(long = "files", value_name = "A,B", value_delimiter = ',', conflicts_with = "files")]
    files_csv: Vec<String>,
    /// Explicit changed files.
    files: Vec<String>,
}

#[derive(Args)]
struct QueryArgs {
    /// Maximum hops from a changed file (0 = unlimited; default from ripple.yml).
    #[arg(long)]
    depth: Option<u32>,
    #[arg(long, value_enum)]
    min_confidence: Option<ConfArg>,
    /// Treat every changed file as wholly changed and ignore method / action
    /// scopes on dependencies (the coarser, wider answer).
    #[arg(long)]
    file_level: bool,
}

#[derive(Args)]
struct ImpactArgs {
    #[command(flatten)]
    diff: DiffArgs,
    #[command(flatten)]
    query: QueryArgs,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// List every dependent, further file, test and mention (default: each
    /// list is cut to a small budget and ends with "+N more").
    #[arg(long, conflicts_with = "limit")]
    all: bool,
    /// List at most N entries per list (counts stay complete).
    #[arg(long)]
    limit: Option<usize>,
    /// Wait until every file change made before this command is indexed
    /// (default: wait at most `daemon.max_wait_ms`, then answer marked stale).
    #[arg(long, conflicts_with = "no_wait")]
    sync: bool,
    /// Answer from the current index at once (may be marked stale).
    #[arg(long)]
    no_wait: bool,
    /// Only this method / controller action of the one file given changed
    /// (repeatable): "what depends on Purchase#refund!".
    #[arg(long = "scope", value_name = "NAME")]
    scopes: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum CheckFormat {
    Github,
    Markdown,
    Json,
    Text,
}

#[derive(Args)]
struct CheckArgs {
    #[arg(long, value_name = "REF")]
    base: String,
    /// Default: github under GitHub Actions, else text.
    #[arg(long, value_enum)]
    format: Option<CheckFormat>,
    /// Exit 1 when the risk is at or above this (default: ci.fail_on).
    #[arg(long, value_enum)]
    fail_on: Option<RiskArg>,
    #[command(flatten)]
    query: QueryArgs,
    /// Also write the markdown report here (e.g. for a PR comment).
    #[arg(long, value_name = "PATH")]
    markdown_out: Option<PathBuf>,
    /// Also write the JSON report here.
    #[arg(long, value_name = "PATH")]
    json_out: Option<PathBuf>,
}

#[derive(Args)]
struct DependentsArgs {
    file: String,
    /// Start from this method / controller action of FILE (repeatable).
    #[arg(long = "scope", value_name = "NAME")]
    scopes: Vec<String>,
    /// Maximum hops (0 = unlimited; default from ripple.yml).
    #[arg(long)]
    depth: Option<u32>,
    #[arg(long, value_enum)]
    min_confidence: Option<ConfArg>,
    /// List everything (default: each list is cut to a small budget).
    #[arg(long, conflicts_with = "limit")]
    all: bool,
    /// List at most N entries per list.
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, value_enum, default_value_t = OutFormat::Text)]
    format: OutFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum RankBy {
    Score,
    Churn,
    Complexity,
    /// Lowest known coverage first.
    Coverage,
}

#[derive(Args)]
struct RankArgs {
    /// Files listed.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Only files matching this glob (repeatable).
    #[arg(long = "path", value_name = "GLOB")]
    path: Vec<String>,
    /// Only files in this area of ripple.yml.
    #[arg(long, value_name = "NAME")]
    area: Option<String>,
    #[arg(long, value_enum, default_value_t = RankBy::Score)]
    by: RankBy,
    #[arg(long, value_enum, default_value_t = OutFormat::Text)]
    format: OutFormat,
    /// Answer at once from the current index, even while git history or
    /// coverage are still loading (default: wait up to 5 s; without a ranking the exit code is 4).
    #[arg(long)]
    no_wait: bool,
}

#[derive(Args)]
struct NeighbourArgs {
    file: String,
    /// Hops to follow (0 = unlimited).
    #[arg(long, default_value_t = 1)]
    depth: u32,
    #[arg(long, value_enum)]
    min_confidence: Option<ConfArg>,
    #[arg(long, value_enum, default_value_t = OutFormat::Text)]
    format: OutFormat,
}

/// Entry point used by `main`. Exit codes: 0 ok, 1 `check` risk at/above
/// threshold, 2 usage or configuration error (also any other failure),
/// 3 `daemon status` found no daemon, 4 `rank` has no ranking yet (signals still loading).
pub fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            let _ = e.print();
            return ExitCode::from(code);
        }
    };
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            if e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe) {
                return ExitCode::SUCCESS;
            }
            eprintln!("ripple: error: {e:#}");
            ExitCode::from(2)
        }
    }
}

pub fn resolve_root(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(r) => r.canonicalize().with_context(|| format!("--root {}", r.display())),
        None => files::find_root(&std::env::current_dir()?),
    }
}

/// Lexically normalises `p` (relative to the cwd) into a root-relative path.
/// Works for files that no longer exist.
pub fn to_rel(root: &Path, p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map(|c| c.join(path)).unwrap_or_else(|_| path.to_path_buf()) };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    let rel = out.strip_prefix(root).ok().map(Path::to_path_buf).or_else(|| {
        // The cwd may be a symlinked spelling of the canonical root.
        let parent = out.parent()?.canonicalize().ok()?;
        Some(parent.join(out.file_name()?).strip_prefix(root).ok()?.to_path_buf())
    });
    match rel {
        Some(r) => r.to_string_lossy().into_owned(),
        None => p.trim_start_matches("./").to_string(),
    }
}

fn stdout_write(s: &str) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(s.as_bytes())?;
    if !s.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn result_of(reply: Reply) -> Result<String> {
    reply.result.map_err(|e| anyhow!("{} ({})", e.message, e.code))
}

fn run(cli: Cli) -> Result<u8> {
    let root = resolve_root(cli.root.as_deref())?;
    if let Some(p) = &cli.config {
        let abs = std::fs::canonicalize(p).with_context(|| format!("--config {}", p.display()))?;
        if !abs.is_file() {
            bail!("--config {}: not a file", p.display());
        }
        ripple_policy::set_config_override(abs);
    }
    let open = |autostart: bool| Backend::open(&root, cli.no_daemon, autostart);
    match cli.cmd {
        Cmd::Impact(a) => impact(&root, a, open(true)?),
        Cmd::Check(a) => check(&root, a, cli.no_daemon),
        Cmd::Dependents(a) => dependents(&root, a, open(true)?),
        Cmd::Dependencies(a) => dependencies(&root, a, open(true)?),
        Cmd::Schema => {
            stdout_write(ripple_policy::schema::REPORT_SCHEMA)?;
            Ok(0)
        }
        Cmd::Why { from, to, scopes, format } => why(&root, &from, &to, &scopes, format, open(true)?),
        Cmd::Areas { file, format } => {
            let mut b = open(true)?;
            let out = result_of(b.call("areas", &json!({"file": to_rel(&root, &file)}))?)?;
            print_value(&out, format, crate::output::areas)
        }
        Cmd::File { file, format, no_wait } => {
            let mut b = open(true)?;
            let out = call_waiting_for_signals(&mut b, "file", &json!({"file": to_rel(&root, &file), "no_wait": no_wait}), !no_wait)?;
            print_value(&out, format, crate::output::file_profile)
        }
        Cmd::Rank(a) => {
            let mut b = open(true)?;
            let by = match a.by {
                RankBy::Score => "score",
                RankBy::Churn => "churn",
                RankBy::Complexity => "complexity",
                RankBy::Coverage => "coverage",
            };
            let out = call_waiting_for_signals(&mut b, "rank", &json!({"limit": a.limit, "path": a.path, "area": a.area, "by": by, "no_wait": a.no_wait}), !a.no_wait)?;
            print_value(&out, a.format, crate::output::rank)?;
            // No ranking yet (signals still loading): not an empty success.
            let ready = serde_json::from_str::<Value>(&out).map(|v| v["ready"] != Value::Bool(false)).unwrap_or(true);
            Ok(if ready { 0 } else { 4 })
        }
        Cmd::Stats { format } => {
            let mut b = open(true)?;
            let out = result_of(b.call("stats", &Value::Null)?)?;
            print_value(&out, format, crate::output::stats)
        }
        Cmd::Init { force } => init(&root, force),
        Cmd::Config { cmd: ConfigCmd::Check } => config_check(&root),
        Cmd::Daemon { cmd } => daemon_cmd(&root, cmd),
        Cmd::History { cmd: HistoryCmd::Build { at } } => crate::historyd::cli_build(&root, at.as_deref()),
        Cmd::History { cmd: HistoryCmd::Status } => {
            let daemon = if cli.no_daemon { None } else { DaemonPaths::for_root(&root).ok().and_then(|p| Client::connect(&p)) };
            let stats = match daemon {
                Some(mut c) => Some(serde_json::from_str::<Value>(&result_of(c.call("stats", &Value::Null)?)?)?),
                None => None,
            };
            crate::historyd::cli_status(&root, stats.as_ref().map(|v| &v["history"]))
        }
        Cmd::Serve { foreground } => serve(&root, foreground),
        Cmd::Bench(a) => crate::bench::run(&root, a),
    }
}

/// How long `rank` and `file` wait for git history, coverage and the
/// ranking to load (a fresh daemon) before answering with what there is.
pub const SIGNAL_WAIT: Duration = Duration::from_secs(5);

/// Calls `method` (`rank`, `file`) and, when `wait`, repeats it for up to
/// [`SIGNAL_WAIT`] while one of the reply's `signals` is still building or
/// loading. They are explicit requests for those signals; `impact` never
/// waits for them.
fn call_waiting_for_signals(b: &mut Backend, method: &str, params: &Value, wait: bool) -> Result<String> {
    let deadline = std::time::Instant::now() + SIGNAL_WAIT;
    loop {
        let out = result_of(b.call(method, params)?)?;
        if !wait || std::time::Instant::now() >= deadline {
            return Ok(out);
        }
        let v: Value = serde_json::from_str(&out)?;
        let loading = v["signals"].as_object().is_some_and(|m| m.values().any(|s| matches!(s["state"].as_str(), Some("building" | "loading"))));
        if !loading {
            return Ok(out);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn print_value(raw: &str, format: OutFormat, text: fn(&Value) -> String) -> Result<u8> {
    match format {
        OutFormat::Json => stdout_write(raw)?,
        OutFormat::Text => stdout_write(&text(&serde_json::from_str(raw)?))?,
    }
    Ok(0)
}

/// The changed files of a request. With a daemon, a git diff is left to
/// the daemon (`diff`), which caches it; in-process it is computed here.
fn diff_params(root: &Path, d: &DiffArgs, backend: &Backend) -> Result<ImpactParams> {
    let mut p = ImpactParams::default();
    let explicit: Vec<String> = d.files_csv.iter().chain(&d.files).filter(|f| !f.is_empty()).map(|f| to_rel(root, f)).collect();
    if !explicit.is_empty() {
        p.files = explicit;
    } else {
        let spec = match (&d.base, d.staged) {
            (Some(b), _) => DiffSpec::Base(b.clone()),
            (None, true) => DiffSpec::Staged,
            (None, false) => DiffSpec::WorkingTree,
        };
        if !files::in_git_repo(root) {
            bail!("not a git repository: pass the changed files explicitly (ripple impact FILE...)");
        }
        if backend.is_daemon() {
            p.diff = Some(DiffRequest::from_spec(&spec));
        } else {
            p.changed = git::changed_files(root, &spec)?;
        }
    }
    Ok(p)
}

/// Sends `impact`. A daemon older than [`PROTOCOL`] 2 ignores `diff`; then
/// the diff is computed here and the request repeated.
fn call_impact(root: &Path, backend: &mut Backend, p: &mut ImpactParams) -> Result<Reply> {
    let reply = backend.call("impact", &serde_json::to_value(&*p)?)?;
    match p.diff.take() {
        Some(d) if reply.protocol < PROTOCOL => {
            eprintln!("ripple: the running daemon is older than this ripple; restart it with `ripple daemon stop`");
            p.changed = git::changed_files(root, &d.spec())?;
            backend.call("impact", &serde_json::to_value(&*p)?)
        }
        Some(d) => {
            p.diff = Some(d);
            Ok(reply)
        }
        None => Ok(reply),
    }
}

fn impact(root: &Path, a: ImpactArgs, mut backend: Backend) -> Result<u8> {
    let mut p = diff_params(root, &a.diff, &backend)?;
    if !a.scopes.is_empty() {
        if p.files.len() != 1 {
            bail!("--scope needs exactly one changed file given explicitly (ripple impact --files FILE --scope NAME)");
        }
        p.scopes = a.scopes.clone();
    }
    p.max_depth = a.query.depth;
    p.min_confidence = a.query.min_confidence.map(Into::into);
    p.file_level = a.query.file_level;
    p.format = a.format;
    p.color = a.format == Format::Text && use_color();
    p.limit = a.limit;
    p.all = a.all;
    p.sync = a.sync;
    p.no_wait = a.no_wait;
    let reply = call_impact(root, &mut backend, &mut p)?;
    let out = result_of(reply)?;
    match a.format {
        Format::Json => stdout_write(&out)?,
        Format::Text | Format::Markdown => stdout_write(&serde_json::from_str::<String>(&out)?)?,
    }
    Ok(0)
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn check(root: &Path, a: CheckArgs, no_daemon: bool) -> Result<u8> {
    // Config errors are usage errors: exit 2 with the message.
    let loaded = match ripple_policy::load_config(root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ripple: {e}");
            return Ok(2);
        }
    };
    let in_actions = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    let format = a.format.unwrap_or(if in_actions { CheckFormat::Github } else { CheckFormat::Text });
    let fail_on = match a.fail_on {
        Some(RiskArg::Never) => None,
        Some(RiskArg::Low) => Some(Risk::Low),
        Some(RiskArg::Medium) => Some(Risk::Medium),
        Some(RiskArg::High) => Some(Risk::High),
        Some(RiskArg::Critical) => Some(Risk::Critical),
        None => loaded.config.ci.fail_on,
    };
    let diff = DiffArgs { base: Some(a.base.clone()), staged: false, files_csv: Vec::new(), files: Vec::new() };
    // CI never autostarts a daemon, but uses one that is already running.
    let mut backend = Backend::open(root, no_daemon, false)?;
    let mut p = diff_params(root, &diff, &backend)?;
    p.max_depth = a.query.depth;
    p.min_confidence = a.query.min_confidence.map(Into::into);
    p.file_level = a.query.file_level;
    p.format = Format::Json;
    // CI sees everything: the full report and the policy section, always.
    p.all = true;
    p.policy = true;
    let raw = result_of(call_impact(root, &mut backend, &mut p)?)?;
    let report: Report = serde_json::from_str(&raw).context("decoding the impact report")?;
    let max_listed = loaded.config.ci.max_listed;
    let risk = report.policy.as_ref().map_or(Risk::Low, |p| p.risk);

    if let Some(path) = &a.markdown_out {
        std::fs::write(path, render::markdown::render(&report, max_listed)).with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(path) = &a.json_out {
        std::fs::write(path, &raw).with_context(|| format!("writing {}", path.display()))?;
    }
    let failing = fail_on.is_some_and(|f| risk >= f);
    match format {
        CheckFormat::Json => stdout_write(&raw)?,
        CheckFormat::Markdown => stdout_write(&render::markdown::render(&report, max_listed))?,
        CheckFormat::Text => stdout_write(&render::text::render(&report, max_listed, std::io::stdout().is_terminal()))?,
        CheckFormat::Github => {
            let summary = std::env::var_os("GITHUB_STEP_SUMMARY").filter(|_| loaded.config.ci.github).map(PathBuf::from);
            let output = std::env::var_os("GITHUB_OUTPUT").map(PathBuf::from);
            let mut stdout = std::io::stdout().lock();
            render::github::emit(&report, max_listed, &mut stdout, summary.as_deref(), output.as_deref())?;
            if failing {
                writeln!(stdout, "::error title=ripple::Change risk is {}, at or above the fail-on level {}", word(risk), word(fail_on.unwrap_or(Risk::Critical)))?;
            }
        }
    }
    if failing {
        if format != CheckFormat::Github {
            eprintln!("ripple: risk {} is at or above the fail-on level {}", word(risk), word(fail_on.unwrap_or(Risk::Critical)));
        }
        return Ok(1);
    }
    Ok(0)
}

fn word(r: Risk) -> String {
    format!("{r:?}").to_lowercase()
}

fn dependents(root: &Path, a: DependentsArgs, mut b: Backend) -> Result<u8> {
    let params = crate::protocol::FileParams {
        file: to_rel(root, &a.file),
        scopes: a.scopes,
        depth: a.depth,
        min_confidence: a.min_confidence.map(Confidence::from),
        all: a.all,
        limit: a.limit,
        sync: false,
        no_wait: false,
        max_wait_ms: None,
    };
    let out = result_of(b.call("dependents", &serde_json::to_value(&params)?)?)?;
    match a.format {
        OutFormat::Json => stdout_write(&out)?,
        OutFormat::Text => {
            let report: Report = serde_json::from_str(&out).context("decoding the report")?;
            stdout_write(&render::text::render(&report, usize::MAX, use_color()))?;
        }
    }
    Ok(0)
}

fn dependencies(root: &Path, a: NeighbourArgs, mut b: Backend) -> Result<u8> {
    let params = json!({"file": to_rel(root, &a.file), "depth": a.depth, "min_confidence": a.min_confidence.map(Confidence::from)});
    let out = result_of(b.call("dependencies", &params)?)?;
    print_value(&out, a.format, crate::output::neighbours)
}

fn why(root: &Path, from: &str, to: &str, scopes: &[String], format: OutFormat, mut b: Backend) -> Result<u8> {
    let out = result_of(b.call("why", &json!({"from": to_rel(root, from), "to": to_rel(root, to), "scopes": scopes}))?)?;
    print_value(&out, format, crate::output::why)
}

fn init(root: &Path, force: bool) -> Result<u8> {
    let path = root.join("ripple.yml");
    if let Some(existing) = ripple_policy::find_config(root).filter(|_| !force) {
        bail!("{} already exists (use --force to overwrite)", existing.display());
    }
    std::fs::write(&path, INIT_TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}\nedit the areas and rules, then run `ripple config check`", path.display());
    Ok(0)
}

pub const INIT_TEMPLATE: &str = include_str!("init_template.yml");

fn config_check(root: &Path) -> Result<u8> {
    let loaded = match ripple_policy::load_config(root) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ripple: {e}");
            return Ok(2);
        }
    };
    let source = loaded.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(no ripple.yml; built-in defaults)".into());
    let policy = match ripple_policy::Policy::compile(loaded.config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ripple: {e}");
            return Ok(2);
        }
    };
    let (files, how) = files::list_files(root, &policy)?;
    println!("{source}: ok");
    println!("workspace: {} files ({} listing)", files.len(), how.as_str());
    let mut counts = vec![0usize; policy.areas().len()];
    let (mut tests, mut globals) = (0, 0);
    for f in &files {
        for a in policy.areas_of(f) {
            counts[a as usize] += 1;
        }
        tests += policy.is_test(f) as usize;
        globals += policy.is_global(f) as usize;
    }
    println!("tests: {tests} files   global_files: {globals} files");
    let mut warnings = 0;
    for (a, n) in policy.areas().iter().zip(&counts) {
        println!("area {:<20} {:<9} {:>6} files  {}", a.name, format!("{:?}", a.tier).to_lowercase(), n, a.owners.join(" "));
        if *n == 0 {
            println!("  warning: area `{}` matches no files", a.name);
            warnings += 1;
        }
    }
    let kind = if policy.uses_default_rules() { "built-in default rules" } else { "rules" };
    println!("{kind}:");
    for r in policy.rules() {
        println!("  {:<32} {:<8} {}", r.id, format!("{:?}", r.risk).to_lowercase(), r.require.join(","));
    }
    if warnings > 0 {
        println!("{warnings} warning(s)");
    }
    Ok(0)
}

fn daemon_cmd(root: &Path, cmd: DaemonCmd) -> Result<u8> {
    let paths = DaemonPaths::for_root(root)?;
    match cmd {
        DaemonCmd::Status => match Client::connect(&paths) {
            Some(mut c) => {
                let out = result_of(c.call("stats", &Value::Null)?)?;
                stdout_write(&crate::output::stats(&serde_json::from_str(&out)?))?;
                Ok(0)
            }
            None => {
                println!("not running (socket {})", paths.socket.display());
                Ok(3)
            }
        },
        DaemonCmd::Stop => match Client::connect(&paths) {
            Some(mut c) => {
                let _ = c.call("shutdown", &Value::Null)?;
                for _ in 0..200 {
                    if Client::connect(&paths).is_none() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                println!("stopped");
                Ok(0)
            }
            None => {
                println!("not running");
                Ok(0)
            }
        },
        DaemonCmd::Start => serve(root, false),
    }
}

fn serve(root: &Path, foreground: bool) -> Result<u8> {
    let paths = DaemonPaths::for_root(root)?;
    if foreground {
        server::serve(root, Pipeline::standard(), paths, ServeOptions { signals: true, ..Default::default() })?;
        return Ok(0);
    }
    if Client::connect(&paths).is_some() {
        println!("already running on {}", paths.socket.display());
        return Ok(0);
    }
    spawn_daemon(root, &paths)?;
    if wait_for_daemon(&paths, Duration::from_secs(10)).is_none() {
        bail!("daemon did not start; see {}", paths.log.display());
    }
    println!("started (socket {}, log {})", paths.socket.display(), paths.log.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    fn uses_every_when_key(src: &str) {
        for key in ["touches:", "touches_tier:", "reaches:", "reaches_tier:", "within:", "impacted_files_over:", "changed_files_over:", "changed_lines_over:", "crosses_boundary:", "touches_global:", "untested:", "min_confidence:", "churn_over:", "complexity_over:", "max_method_complexity_over:", "coverage_under:", "changed_lines_coverage_under:", "uncovered:", "uncovered_includes_unknown:", "hotspot:", "missing_cochange:", "cochange_min_confidence:"] {
            assert!(src.contains(key), "missing {key}");
        }
    }

    #[test]
    fn init_template_is_valid() {
        let cfg = ripple_policy::parse_config(INIT_TEMPLATE, "init_template.yml").unwrap();
        assert_eq!(cfg.ci.fail_on, Some(Risk::Critical));
        ripple_policy::Policy::compile(cfg).unwrap();
        uses_every_when_key(INIT_TEMPLATE);
    }

    #[test]
    fn example_config_is_valid() {
        let src = include_str!("../../../examples/ripple.yml");
        let cfg = ripple_policy::parse_config(src, "examples/ripple.yml").unwrap();
        assert!(cfg.areas.contains_key("billing") && cfg.areas.contains_key("auth"));
        ripple_policy::Policy::compile(cfg).unwrap();
        uses_every_when_key(src);
    }

    #[test]
    fn to_rel_normalises() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(to_rel(&root, "./a/../b/c.rb"), "b/c.rb");
        assert_eq!(to_rel(&root, &root.join("x/y.ts").to_string_lossy()), "x/y.ts");
        assert_eq!(to_rel(&root, "gone/deleted.rb"), "gone/deleted.rb");
        assert_eq!(to_rel(&root, "/elsewhere/z.rb"), "/elsewhere/z.rb");
    }
}
