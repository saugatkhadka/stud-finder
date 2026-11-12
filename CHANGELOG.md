# Changelog

ripple releases are tagged `ripple-vX.Y.Z`. The Ruby gem that preceded
ripple stays at tag `v0.7.0` and is not listed here.

## ripple-v0.1.0

First release of stud-finder, ripple edition: a Rust binary that replaces
the Ruby gem on `main`.

**Analysis**

- Dependency graph for Rails + React repositories: Ruby constants and
  `require` (Rubydex resolver), Rails conventions (associations, callbacks,
  views, routes to controllers), ERB/HAML/Slim templates, JS/TS imports
  (oxc), frontend HTTP calls matched to Rails routes, React components
  mounted from views.
- `impact`, `dependents` with method-level narrowing for Ruby methods and
  controller actions, `also mentions` for names with no edge, a `blind
  spots` line with the `rg -w` search to run.
- Metrics per changed file: code lines, cyclomatic complexity, churn,
  coverage from existing SimpleCov, LCOV and Istanbul reports, hotspot
  rank. `file` and `rank` commands.
- Co-change ("usually changes with") from git history, off for short or
  sweep-dominated histories.

**Zero configuration**

- 8 built-in areas and 17 built-in `default.*` rules, calibrated on
  Mastodon and Gumroad (HIGH or above on 8.3% and 16.9% of commits).
- Auto-detection of test layouts, CODEOWNERS and coverage reports.
- `ripple.yml` extends, overrides or disables the built-ins; relative
  `when` keys (`*_top_percent`, `impacted_files_over_percent`).
- `config show`, `config check`, `init` (every key spelled out, commented,
  including `daemon.sleep_after_seconds`), `doctor`.

**CI and Unix**

- `impact --fail-on LEVEL|ACTION,…` and `check`; exit codes 0/1/2 (plus 3
  and 4 for `daemon status` and `rank`).
- `--format text|json|markdown|github`; GitHub annotations, job summary
  and step outputs. Example workflows in `examples/`.
- Paths on stdin (`impact -`, `file -`), JSON Schemas (`ripple schema`,
  copies in `docs/`), `NO_COLOR`, `-q`, shell completions.

**Agents**

- `ripple skill install|update|uninstall|status|print` for Claude, Codex
  and `.agents` layouts, project or user scope; never overwrites an edited
  skill without `--force`.

**Daemon**

- One daemon per repository and config: file watcher, incremental
  re-indexing (body-only and reference-only Ruby edits skip the resolver),
  cookie-synced freshness with a 150 ms wait before answering stale,
  `--sync` / `--no-wait`.
- Sleep after 5 minutes idle: the index is saved to
  `<git dir>/ripple/index-<hash>.snap`, the daemon re-executes itself and
  serves from the memory-mapped file (6 MB on Mastodon, 18 MB on GitLab).
  When a snapshot is saved, other snapshots in that directory are removed
  if they are at least 10 minutes old and either older than 3 days or
  beyond the newest 3.
- macOS: the daemon runs with `MallocSpaceEfficient=1` (about 40% less
  memory awake).

**Reports**

- Three or more plain non-source files changed in one directory (locale
  JSON, fixtures) collapse to one line in text and markdown output; JSON
  still lists every file.

**Install**

- `install.sh` (checksum-verified, no sudo) and release binaries for macOS
  arm64/x86_64 and Linux x86_64/arm64.
