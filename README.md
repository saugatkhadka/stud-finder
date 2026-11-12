# ripple

Change impact analysis for Rails + React monorepos. For a set of changed
files, ripple lists every file that depends on them, directly or through
other files, and turns that blast radius into a risk level and a list of
required actions using rules you write in `ripple.yml`.

Dependencies come from:

- Ruby constants and `require`
- Rails conventions (associations, views, routes → controllers)
- JS/TS imports
- frontend HTTP calls matched to Rails routes

ripple answers from a dependency graph that a per-repository background
daemon keeps up to date as files change. A query usually takes tens of
microseconds on the server.

```
$ ripple impact --base main
CRITICAL risk  1 changed, 5 impacted (depth ≤4), 1 test to run  gen 1
  CRITICAL billing-touched Touches area billing: `app/models/billing/invoice.rb`
  HIGH     critical-direct-dependency Reaches 1 file in billing (critical) within 1 hop
           app/services/billing/charge_service.rb -> (ruby_constant `Billing::Invoice` line 4) app/models/billing/invoice.rb
  MEDIUM   frontend-backend-contract Crosses the Ruby/JS boundary: 2 js files on the other side depend on the change
           app/javascript/checkout/pay.ts -> (api_call `POST /api/charges → api/charges#create` line 2 inferred) app/controllers/api/charges_controller.rb -> ...
require: frontend_review, human_review, payments_review
...
```

## Quickstart

```sh
cargo build --release -p ripple-cli          # binary: target/release/ripple
cd your-rails-app
ripple init                                  # writes a commented ripple.yml
ripple config check                          # validates it and shows what each area matches
ripple impact                                # working tree vs HEAD (untracked files included)
ripple impact --base main                    # everything since merge-base(main, HEAD)
ripple impact app/models/user.rb             # explicit files
ripple dependents app/models/user.rb --depth 0
ripple impact --files app/models/purchase.rb --scope refund!   # only what reaches Purchase#refund!
ripple why app/javascript/checkout/pay.ts app/models/billing/invoice.rb
```

The first command starts the daemon for the repository. Later commands reuse
it. For CI, see [`examples/github-workflow.yml`](examples/github-workflow.yml).

## Commands

| Command | What it does |
|---|---|
| `impact [FILES] [--base REF \| --staged \| --files A,B]` | Impact report. `--format text\|json\|markdown`, `--depth N` (0 = unlimited), `--min-confidence exact\|inferred\|heuristic`, `--limit N` (cap the listed files; counts stay exact), `--sync` (also wait for unrelated pending edits), `--file-level` (see [Method-level analysis](#method-level-analysis)), `--scope NAME` (repeatable; with exactly one file: only these methods / controller actions changed). |
| `check --base REF` | CI entry point. `--format github\|markdown\|json\|text` (github is the default under `GITHUB_ACTIONS`), `--fail-on low\|medium\|high\|critical\|never` (default `ci.fail_on`), `--markdown-out PATH`, `--json-out PATH`, `--file-level`. Never starts a daemon, but uses one that is already running. |
| `dependents FILE` / `dependencies FILE` | Direct neighbours by default; `--depth N` (0 = all). `dependents --scope NAME`: only what depends on that method / action. |
| `why FROM TO` | Shortest dependency path between two files, in either direction. `--scope NAME`: only paths into that method / action of TO (says so when only a file-level path exists). |
| `areas FILE` | How `ripple.yml` classifies a file: areas, test, global, language. |
| `stats` | Index size, scope ranges (`files_with_scopes`, `scopes`), per-phase timings, extractor counters, warnings, daemon info. |
| `init [--force]`, `config check` | Starter config; validation plus how many files each area matches. |
| `daemon status\|start\|stop`, `serve [--foreground]` | Manage the daemon. |

Global flags: `--root DIR` (or `RIPPLE_ROOT`; default is the enclosing git
work tree) and `--no-daemon` (index in this process).

Paths can be given relative to the current directory or as absolute paths.
They are normalised to paths relative to the repository root.

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | `check` found a risk at or above the fail-on level |
| 2 | Usage error, configuration error, bad git ref, file not in the index, or internal error |
| 3 | `daemon status` found no daemon running |

### What `--base` compares

`--base REF` compares `merge-base(REF, HEAD)` with the working tree. It
covers:

- committed changes, including renames (detected with `-M`) and deletions
- staged changes
- unstaged changes
- untracked files that are not ignored

The default with no flags is HEAD compared with the working tree, plus
untracked files. Every changed file is reported, including files that are not
in the graph. Those get a warning, because their impact is unknown, not
empty.

## Configuration reference (`ripple.yml` or `.ripple.yml`)

A missing file means built-in defaults. Unknown keys, invalid values and semantic
problems are errors that name the key and the line. Semantic problems are
collected and reported together:

```
ripple: ripple.yml: line 4, column 11: at `areas.billing.tier`: unknown variant `extreme`, expected one of `low`, `medium`, `high`, `critical`
ripple: ripple.yml: invalid configuration:
  - rules[0].when.touches[0] (line 3): unknown area `nope` (known areas: none defined); globs must contain `/`, `*` or `.`
  - rules[1].id (line 6): duplicate rule id `a` (also rules[0])
```

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Must be 1. |
| `ignore` | node_modules, vendor, tmp, log, public, coverage, dist, build | Globs that are never analysed or reported. These are in addition to `.gitignore`. |
| `areas.<name>.paths` | — | Globs for the files in the area. |
| `areas.<name>.tier` | required | One of `low`, `medium`, `high`, `critical`. |
| `areas.<name>.owners` | `[]` | Suggested reviewers when a change touches or reaches the area. |
| `areas.<name>.description` | — | Free text. |
| `global_files` | initializers, application.rb, environments, boot.rb, Gemfile(.lock), package.json, lockfiles | Files that the whole app depends on in ways no static graph can see. A change to one is reported as "global: static impact is not reliable for this file". |
| `tests` | `spec/**`, `test/**`, `__tests__`, `*.test.*`, `*.spec.*`, `*_spec.rb`, `*_test.rb`, `*.stories.*` | Test files. They go into "tests to run" and are never counted in the blast radius. |
| `rules[]` | built-in defaults (below) | Each rule has `id`, `when` (required; `{}` always fires), `risk`, `require: [..]` and an optional `description`. The resulting risk is the highest risk among the rules that fired, and the `require` lists are merged. |
| `query.max_depth` | `3` | How many hops to follow; `0` = unlimited. In a densely connected Rails app the unlimited closure of most files is most of the app. |
| `query.min_confidence` | `heuristic` | Edges below this confidence are ignored. |
| `ci.fail_on` | none | `check` exits 1 at or above this risk. |
| `ci.github` | `true` | Write `$GITHUB_STEP_SUMMARY` under Actions. |
| `ci.max_listed` | `50` | Files listed in the markdown and text output; it ends with "…and N more". |
| `daemon.idle_timeout_minutes` | `30` | The daemon exits after this many minutes without a request; `0` = never. The next command starts a new one. |
| `ruby`, `js`, `boundary` | — | Extractor settings: `enabled`, autoload roots, tsconfig/aliases, route files, HTTP callees and URL bases. Changing one of these makes the daemon rebuild from scratch. Changing other keys only reloads the policy. |

**Globs** follow gitignore conventions:

| Pattern | Matches |
|---|---|
| `*` | Stays within one directory |
| `**` | Crosses directories |
| A pattern without `/` | Any depth (`*.log`) |
| A leading `/` | Anchored to the repository root (`/Gemfile`) |
| A directory pattern | Everything under that directory (`app/models/billing`) |
| A trailing `/` | Directories only |

In `touches` and `reaches`, a name that matches an area refers to that area.
Anything containing `/ * ? [ { .` is treated as a glob. Any other name is an
"unknown area" error.

### `when` conditions

All the conditions in a rule must hold for it to fire. A rule with an empty
`when` always fires. Every key is shown in
[`examples/ripple.yml`](examples/ripple.yml).

| Key | Holds when |
|---|---|
| `touches: [area\|glob]` | A changed file is in a listed area or matches a listed glob. For renames, the old path counts too. |
| `touches_tier: T` | A changed file is in an area of tier T or higher. |
| `reaches: [area\|glob]` | An impacted file (depth ≥ 1, not a test, not itself changed) is in a listed area or matches a listed glob. |
| `reaches_tier: T` | The same, for an area of tier T or higher. When both `reaches` and `reaches_tier` are given, they are OR-ed. |
| `within: N` | Limits `reaches` and `reaches_tier` to N hops. The default is `query.max_depth`. |
| `impacted_files_over: N` | There are more than N impacted non-test files. Changed files are not counted. |
| `changed_files_over: N` | There are more than N changed files, not counting ignored ones. |
| `changed_lines_over: N` | There are more than N added plus deleted lines. Never fires without a diff (for example with `impact FILES`). |
| `crosses_boundary: true\|false` | An impacted file is on the other side of the Ruby/JS boundary from the changed file it traces back to. Ruby files and templates are the server side; JS/TS files are the client side. |
| `touches_global: true\|false` | A changed file matches `global_files`. |
| `untested: true\|false` | A changed, non-deleted source file has no test among its transitive dependents. |
| `min_confidence: C` | For this rule, ignores edges below C in `reaches*`, `impacted_files_over`, `crosses_boundary` and `untested`. |

**Built-in default rules**, used only when `rules` is absent or empty:

| Rule | Condition | Risk | Requires |
|---|---|---|---|
| `default.touches_critical` | Touches an area of tier critical | critical | `human_review` |
| `default.touches_high` | Touches an area of tier high or above | high | `human_review` |
| `default.touches_global` | Touches a global file | high | `run_full_suite` |
| `default.reaches_critical_direct` | Reaches a critical area within 1 hop | high | — |
| `default.reaches_critical` | Reaches a critical area at any depth | medium | — |
| `default.impacted_over_200` | More than 200 impacted files | high | — |
| `default.impacted_over_50` | More than 50 impacted files | medium | — |
| `default.crosses_boundary` | Crosses the Ruby/JS boundary | medium | — |

If no rule fires, the risk is low.

### Method-level analysis

Extractors can report, per file, the line ranges of its methods and controller
actions (`Facts.scopes`), and, per edge, which method of the dependent it sits
in (`from_scope`) and which method of the dependency it uses (`to_scope`; the
class body is `@body`). When both are known, an edit inside `Purchase#refund!`
seeds only `refund!`: callers of `charge!` are not impacted. Everything is
conservative, so method-level results are always a subset of file-level ones:

- The diff is read with `git diff -U0`; only new-side hunks matter. A hunk
  with new lines `[a, b]` belongs to method `m` iff `m.start < a` and
  `b <= m.end` (the `def` line itself is not "inside": renaming or changing a
  signature breaks callers by name). A pure deletion after new line `p`
  belongs to `m` iff `m.start <= p` and `p + 1 <= m.end`. Nested methods: all
  containing ranges are seeds.
- The whole file is the seed when: the file was added, deleted or renamed; it
  is binary or has no textual diff; it has no scope ranges; any hunk falls
  outside every method (class body, between methods); a hunk contains a
  method's `def` line; or the removed side of a hunk contains a `def` line (a
  method was deleted or merged). `--files` without `--scope` is whole-file,
  and so is `--staged` for files that also have unstaged edits (the line
  numbers would not match the indexed working tree).
- Hunks are computed by the client and mapped onto scopes by whoever holds the
  index (the daemon or the in-process engine), so line numbers always match
  what was indexed.
- Every query uses the narrowed seeds: reach, `touches` of other files,
  `untested` (a file counts as tested if a test reaches one of its changed
  methods), `crosses_boundary`. The per-file `dependents` count stays the
  file-level fan-in.
- `--file-level` turns all of this off (seeds and edge scopes). The report
  always carries `impacted.file_level_total`, and the markdown headline reads
  e.g. "**212** impacted files (method-level; 1,362 if whole files are assumed
  changed)". The footer states which mode was used.

## JSON report (`--format json`, `schema_version: 1`)

The report is deterministic: the same index and inputs give byte-identical
output, apart from `timing`. All lists are sorted. Abridged:

```jsonc
{
  "schema_version": 1,
  "risk": "critical",                       // low | medium | high | critical
  "summary": "CRITICAL risk: 1 changed file, 5 impacted files (up to 4 hops), 1 test to run, 5 rules fired",
  "reasons": [{
    "rule": "billing-touched", "risk": "critical", "message": "Touches area billing: `app/models/billing/invoice.rb`",
    "description": "…", "require": ["human_review"], "builtin": false,
    "evidence": {
      "changed": ["app/models/billing/invoice.rb"],
      "impacted": [{ "file": "…", "depth": 1, "path": [{ "file": "…", "kind": "ruby_constant", "confidence": "exact", "line": 4, "detail": "Billing::Invoice" }, { "file": "…" }] }],
      "impacted_matching": 1
    }
  }],
  "require": ["human_review", "payments_review"],
  "changed": [{ "path": "…", "status": "modified|added|deleted|renamed", "old_path": "…", "added": 5, "deleted": 1, "first_line": 4,
                "scopes": ["refund!"],              // only these methods changed; absent = the whole file
                "language": "ruby|js|template|other", "areas": ["billing"], "dependents": 2, "in_graph": true, "is_test": false, "is_global": false }],
  "impacted": {
    "total": 5, "file_level_total": 9, "tests": 1, "max_depth": 4,  // file_level_total: impacted if whole files had changed
    "by_depth": { "1": 1, "2": 1 }, "by_language": { "ruby": 2 }, "by_edge_kind": { "ruby_constant": 2 }, "by_min_confidence": { "exact": 3, "inferred": 2 },
    "files": [{ "file": "…", "depth": 1, "via": "…", "root": "<changed file it traces to>", "kind": "ruby_constant", "confidence": "exact",
                "path_confidence": "<weakest edge on the path>", "line": 4, "detail": "Billing::Invoice", "language": "ruby", "areas": ["billing"],
                "from_scope": "run", "to_scope": "refund!" }],   // methods the first edge sits in / uses, when known (never `@body`)
    "truncated": false                     // true when --limit cut `files` / `tests_to_run`
  },
  "tests_to_run": [ /* same shape as impacted.files; changed test files have depth 0 */ ],
  "areas_reached": [{ "area": "billing", "tier": "critical", "owners": ["@acme/payments"], "touched": true, "files": 2, "min_depth": 0, "example": { /* path */ } }],
  "owners": ["@acme/payments"],
  "warnings": [{ "code": "global_file_changed", "message": "global: static impact is not reliable for this file …", "file": "config/initializers/money.rb" }],
  "query": { "max_depth": 0, "min_confidence": "heuristic", "file_level": false },
  "timing": { "refresh_us": 6, "query_us": 5, "evaluate_us": 9 },
  "index": { "generation": 1, "nodes": 61037, "edges": 397567, "pending_changes": 0 }
}
```

Warning codes include:

- `global_file_changed`
- `changed_file_not_indexed`
- `deleted_file_not_indexed`
- `changed_file_ignored`
- `no_line_counts`
- `scope_unknown` (`--scope` names a method missing from the file's scope ranges)
- `deleted_file_still_imported` (cold index only: JS/TS files whose imports of
  a deleted file no longer resolve)
- `config_invalid`
- extractor warnings about the changed files themselves

## Daemon and freshness

- **One daemon per repository.** It is started on demand by any command
  except `check`. Autostart is off when `RIPPLE_NO_DAEMON=1`, `CI` or
  `GITHUB_ACTIONS` is set; `--no-daemon` indexes in-process.
  - The socket, lock file and log live in `$RIPPLE_SOCKET_DIR`, else
    `$XDG_RUNTIME_DIR/ripple`, else `/tmp/ripple-$UID`. The directory has mode
    0700 and must be owned by you. The files are named
    `<fnv1a64(canonical root)>.sock|.lock|.log`. Socket paths are limited to
    100 bytes; if the path would be longer, ripple says so and runs
    in-process.
  - An exclusive `flock` on the `.lock` file makes the daemon a singleton.
    Because a running daemon always holds the lock, a socket left behind by a
    killed daemon (`kill -9`) is recognised as stale, and the next daemon
    removes it. SIGTERM, SIGINT and SIGHUP remove the socket before the
    daemon exits.
- **Indexing.**
  - Files come from `git ls-files -c -o --exclude-standard` minus deleted
    files; outside git, ripple walks the tree with the `ignore` crate.
  - The extractors run in parallel, then the linkers, then the graph is
    built. Each result is published as an immutable snapshot that readers
    pick up atomically.
  - File events from `notify` (FSEvents or inotify) are debounced: a batch is
    indexed after 30 ms with no new events, or at most 300 ms after its first
    event. Each batch goes through the extractors' incremental `update`.
  - When `ripple.yml` changes, the policy is reloaded. If an extractor
    setting changed, everything is rebuilt.
- **Freshness contract.** Before the daemon answers a request, it:
  1. folds in every file event already delivered;
  2. re-stats the files named in the request, using mtime against the time
     each file was last examined;
  3. waits until both are indexed. The request is moved to the front of the
     debounce queue.

  As a result, a query that names a file always sees that file's current
  contents. This is tested with a 200-iteration edit-then-query loop that
  has no sleeps. `--sync` (`"sync": true`) also writes a cookie file in
  `.git/` and waits for the watcher to report it. That covers edits to files
  the request does not name. Freshness is not just about the named files:
  editing another file can add an edge to the named one.
- **Diffs are computed in the daemon.** For `impact --base`, `--staged` and
  the default work-tree diff, the client sends the diff spec (`"diff"`) and
  the daemon answers it from a cache. It first writes a cookie (as for
  `--sync`), so every file event before the request is seen. Then it re-runs
  git only for the paths that changed since the last answer: the watcher
  reports work-tree paths, and the daemon reads `.git/index` itself to find
  staged changes. Commits, checkouts, resets, stash and ref updates are
  detected from `HEAD`, the refs and `packed-refs`, and the merge base is
  cached per ref value. Changes to `.gitignore`, `.gitattributes`, git
  config, `info/exclude`, or more than 1,000 paths at once, trigger a full
  recompute. So does a watcher overflow. The result is the same as the
  client-side `git diff`, which one-shot mode (`--no-daemon`, CI) still uses.
  The daemon runs git with `GIT_OPTIONAL_LOCKS=0`, so it never rewrites the
  index.
- **Requests never wait for unrelated work.** Each connection has its own
  thread (at most 128 at once; more clients wait in the listen queue). A
  connection that sends nothing for 10 minutes is closed. If a re-index is already running and none of the request's files
  are pending, the daemon answers from the current snapshot at once and
  reports `index.pending_changes`.
- **Deletions and renames.** While the daemon is running, a deleted file
  keeps the edges from its former dependents (their `detail` is prefixed
  `deleted:`). Its impact therefore still lists the files whose references
  are now broken. A cold index (CI, `--no-daemon`) cannot know these edges.
  It reports the deleted file with a `deleted_file_not_indexed` warning
  instead of silently reporting no impact. A rename is a deletion plus an
  addition. Both paths are seeds, and `touches` matches either one.
- **Case-insensitive file systems** (macOS): a wrongly-cased name never
  becomes a second workspace file.

### Protocol

The protocol is newline-delimited JSON over the Unix socket. Each request
gets one response line, in order, per connection:

```
→ {"id": 1, "method": "impact", "params": {"files": ["app/models/user.rb"], "format": "json", "limit": 50}}
← {"id": 1, "ok": true, "generation": 7, "server_us": 41, "result": { …report… }}
← {"id": 2, "ok": false, "generation": 7, "server_us": 12, "error": {"code": "not_in_graph", "message": "…"}}
```

Methods:

| Method | Params |
|---|---|
| `impact` | `files`, `changed` (entries may carry `hunks`: `[{start, count, removes_definition?}]`), `diff` (`{"base": "REF"}`, `{"staged": true}` or `{}` for the work tree; not combined with `files`/`changed`), `scopes`, `file_level`, `max_depth`, `min_confidence`, `format`, `color`, `limit`, `max_listed`, `sync` |
| `dependents`, `dependencies` | `file`, `scopes` (dependents only), `depth`, `min_confidence` |
| `why` | `from`, `to`, `scopes` |
| `areas` | `file` |
| `stats` | — |
| `reload` | — |
| `shutdown` | — |

Every response carries `"protocol": 2`. A response without it comes from an
older daemon, which ignores `diff`: the client then computes the diff itself
and asks again with `changed`, and prints a hint to restart the daemon.

The full description is in the module docs of
`crates/ripple-cli/src/protocol.rs`.

## Performance

These numbers were measured on an Apple Silicon Mac with a release build, on
the gitlabhq checkout: 61,037 files after ignores, plus 400,000 synthetic
edges. The edges form a skewed acyclic graph (`ripple bench gen-edges`), of
which 397,567 remain after deduplication. Only the edge-file extractor ran
(`RIPPLE_EXTRACTORS=edge_file`), so these numbers measure ripple itself, not
the language extractors.

| Measurement | Result |
|---|---|
| File listing (`git ls-files`, 10 runs) | p50 354 ms (the untracked-file scan is about 250 ms of it) |
| Cold build: list + read edges + graph + classify | 815 ms (list 349, edge file 99, graph 347, classify 4.4) |
| Incremental pass for 1 changed file | 423 ms (graph rebuild 308, edge-file reread 91) |
| Daemon RSS after build | 219 MB |
| `server_us`, small seeds (fan-in 1–3, median 6 impacted), 1000 queries | p50 24 µs, p99 91 µs |
| `server_us`, hub seeds (about 41,700 impacted), 1000 queries, full JSON (17.6 MB) | p50 22.3 ms, p99 31.6 ms |
| `server_us`, hub seeds, `limit: 50` | p50 2.7 ms, p99 4.1 ms |
| End to end: `ripple impact --files X --format json`, small seed, 200 runs | p50 2.7 ms, p99 11.9 ms |
| End to end: hub seed with `--limit 50`, 200 runs | p50 5.8 ms, p99 18.9 ms |
| End to end: hub seed, full 17.6 MB JSON, 50 runs | p50 53 ms |
| Autostart plus first answer (cold) | 0.77–1.07 s |

Text and markdown output build only the `max_listed` entries they show, so
they are as cheap as `--limit`. The incremental pass cost is dominated by
`Graph::build`, which rebuilds the whole graph on every pass.

To reproduce, set `RIPPLE_SOCKET_DIR` to a short directory first:

```sh
ripple --root $REPO bench list-files --runs 10
ripple --root $REPO bench gen-edges --out /tmp/edges.txt --edges 400000
export RIPPLE_EXTRACTORS=edge_file RIPPLE_EDGE_FILE=/tmp/edges.txt
ripple --root $REPO bench build
ripple --root $REPO daemon start
ripple --root $REPO bench server --edges /tmp/edges.txt --queries 1000 [--limit 50]
ripple --root $REPO bench e2e --file <path> --runs 200 [--limit 50]
ripple --root $REPO bench query --file <path> [--limit 50]   # in-process: BFS / evaluate / render
```

### Extractors on real repositories

> **Placeholder.** These are to be filled in by the extractor owners. No
> numbers below have been measured yet.

| Repo | Files | Edges | ruby (ms) | js (ms) | boundary (ms) | Cold build (ms) | Incremental, 1 file (ms) | RSS (MB) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| mastodon | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| gumroad | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |
| gitlabhq | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ | _TBD_ |

`ripple --no-daemon stats` prints all of these as `last pass:` timings and
per-extractor counters.

## Limits

- Static analysis only. Metaprogramming, `send`, string-built constants,
  dynamic imports and runtime configuration are invisible. `global_files`
  covers the worst cases.
- A cold index cannot list the dependents of a deleted file. Only a running
  daemon keeps them, as tombstones. For JS/TS the cold report does list the
  files whose imports of it no longer resolve (`deleted_file_still_imported`);
  Ruby constant references to a deleted file are not recoverable cold.
- Method-level narrowing is only as good as the scope data: files without
  scope ranges, and edges without a `to_scope`, count as whole-file.
- Each incremental pass rebuilds the whole graph. On a 400k-edge graph that
  costs about 0.3 s. Queries that do not name a pending file are not blocked
  during the pass.
- A daemon from an older ripple binary keeps running until it is stopped or
  its idle timeout passes.
- Git submodules are not indexed. Symlinks are taken as git lists them.
- Unix only (Unix sockets, `flock`).

## Environment

| Variable | Effect |
|---|---|
| `RIPPLE_ROOT` | Repository root, the same as `--root`. |
| `RIPPLE_NO_DAEMON=1` | Disables daemon autostart. `CI` and `GITHUB_ACTIONS` do the same. |
| `RIPPLE_SOCKET_DIR` | Directory for the socket, lock and log files. |
| `NO_COLOR` | Plain text output. |
| `RIPPLE_EDGE_FILE` | Extra edges from a file: `FROM TO [KIND [CONF [LINE [DETAIL]]]]` per line; either endpoint may be `path#method` (`#@body` = class body); `@inherit FROM TO ...` adds an inheritance edge; `@scope FILE NAME START END` declares a method's line range. `#` after whitespace starts a comment. For tests, benchmarks and custom integrations. |
| `RIPPLE_EXTRACTORS` | Comma list of the extractors and linkers to run, by name. For debugging and benchmarks. |
| `RIPPLE_DEBUG` | Logs watcher events and indexing batches to the daemon log. |
