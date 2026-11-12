# Architecture

How ripple turns a repository into answers, and how the daemon keeps them
fresh. Module docs in the source go deeper; this page names them.

## Crates

| Crate | Role |
|---|---|
| `ripple-core` | The model (`RelPath`, nodes, `Edge`, warnings), `ripple.yml` parsing and merging (`config.rs`), the immutable graph and the impact query (`graph.rs`), method mentions, metrics, and the on-disk section format (`store.rs`). |
| `ripple-ruby` | Ruby and Rails edges: the [Rubydex](https://github.com/Shopify/rubydex) resolver for constants, ruby-prism for Rails facts (associations, callbacks, views, routes), templates (ERB, HAML, Slim), method-level scopes, cyclomatic complexity. |
| `ripple-js` | JS/TS imports, exports and complexity, parsed with oxc. |
| `ripple-boundary` | Edges across the Ruby/JS line: Rails routes, frontend HTTP calls matched to routes, React components mounted from views. |
| `ripple-policy` | Areas, rules, risk levels and actions; the report model and its renderers (text, markdown, github, json). |
| `ripple-history` | Churn and co-change from `git log`, with a binary sidecar cache. |
| `ripple-coverage` | Reads SimpleCov, LCOV and Istanbul reports. |
| `ripple-eval` | The evaluation binary (`ripple-eval breaks` reproduces the dependents numbers in the README). |
| `ripple-cli` | The `ripple` binary: the indexing engine (`engine.rs`), the daemon (`server.rs`), git change tracking (`gitd.rs`), history and coverage loaders (`historyd.rs`, `signals.rs`), the sleep snapshot (`snapfile.rs`), the agent skill (`skill.rs`) and the client (`client.rs`). |

## Data flow

```
                 git ls-files + untracked, minus ignore globs
                                  │
              ┌───────────────────┼────────────────────┐
              ▼                   ▼                    ▼
        ripple-ruby          ripple-js          ripple-boundary
   (Rubydex + prism facts)    (oxc)       (routes, HTTP calls, mounts)
              └───────────────────┼────────────────────┘
                                  ▼  edges, method mentions, metrics
                    Graph (CSR, both directions) ──► Snapshot (Arc, immutable)
                                                          │
   request ─► client ─► Unix socket ─► daemon ─► freshness step ─► query
   (impact, file, rank …)                     (cookie sync, wait ≤150 ms)
                                                          │
          changed files (gitd) ─► reverse BFS ─► policy (areas, rules)
          signals (history, coverage, rank) ─────────────┤
                                                          ▼
                                     report model ─► text | markdown | github | json
```

Without a daemon (`--no-daemon`, CI) the same engine runs in the process:
list, extract, build, query, exit.

## Edge direction and impact

`Edge::new(from, to)` means **`from` depends on `to`** (`from` mentions a
constant defined in `to`, imports it, renders it, calls its route). The
graph stores both adjacency directions in compressed-sparse-row arrays.

- **Impact** is a breadth-first search over the reverse edges: from each
  changed file to the files that depend on it, then to theirs, up to
  `query.max_depth` hops (3). Hop 1 is `dependents`, hops 2–3 are
  `further`, test files anywhere are `tests`.
- **Relies on** is one hop forward.
- **Method-level narrowing.** When the diff touches only some Ruby methods
  or controller actions, hop 1 is cut to files that mention those names
  (plus files that only inherit or include the changed module, reported
  separately); the search continues from there. `impacted_file_level` is
  the unnarrowed count.
- `why FROM TO` is the shortest forward path between two files.

## The daemon

One daemon per repository root and `--config` file. Its socket, lock and
log are `<dir>/<hash>.{sock,lock,log}`, where `<hash>` is a 64-bit FNV-1a
hash of the root and config path, and `<dir>` is `$RIPPLE_SOCKET_DIR`, else
`$XDG_RUNTIME_DIR/ripple`, else `/tmp/ripple-$UID`. The first command
starts it (never under `CI`, `GITHUB_ACTIONS` or `RIPPLE_NO_DAEMON=1`); it
stops after `daemon.idle_timeout_minutes` (30) without a request, or on
`ripple daemon stop`, SIGTERM, SIGINT or SIGHUP (which remove the socket).

Threads: one accept loop; one thread per connection (at most 128 at once);
one indexer thread that owns the engine; the file watcher (`notify`:
FSEvents on macOS, inotify on Linux); background loaders for git history,
coverage and the hotspot ranking; an idle timer. Queries never wait for a
lock the indexer holds: the current snapshot is an `Arc` swapped behind a
lock held only to clone or replace it.

**Freshness.** Before answering, the daemon writes a cookie file and waits
until the watcher reports it (on macOS followed by 24 cheap events, because
FSEvents otherwise delivers small batches after a 10 ms timer, and done in
two rounds), so it has seen every edit made before the request. It then
waits for pending edits to be indexed for at most `daemon.max_wait_ms`
(150 ms). If they are not, it answers from the current index and marks the
answer stale (`INCOMPLETE stale_index`; still-pending changed files count
as wholly changed). `--sync` waits up to 300 s; `--no-wait` does not wait.

**Batching.** After an edit the indexer waits for 30 ms of quiet (at most
300 ms in all, and not at all when a request is waiting), then takes every
path delivered so far as one batch: N editors making M edits cost one pass
per indexer cycle, not N × M passes.

**Changed files** (`gitd.rs`). The one-shot path runs `merge-base`, two
`git diff`s and `git ls-files -o` per call. The daemon caches both results
and re-runs git with a pathspec of only the paths the watcher reported
since, falling back to a full recomputation when HEAD, the target, ignore
rules or the index format change, or more than 1,000 paths changed.

## Incremental paths

`Engine::apply_paths` re-extracts only changed files. For Ruby:

1. **Body-only edits** (method bodies, comments, DSL arguments): each file's
   definition fingerprint is unchanged, so the Rubydex merge and resolve are
   skipped and only that file's edges are recomputed.
2. **Reference-only edits**: the definitions keep their fingerprint and the
   plain constant references name the same set of names in the same order
   relative to superclass and mixin references. Same fast path, because
   Rubydex resolves names, not positions.
3. **Structural edits** (a new class, a new constant reference, a changed
   superclass): the file's local graph is merged into the long-lived
   Rubydex graph and the resolver runs incrementally over the pending work;
   then edges are recomputed for all files (cheap next to resolution).
4. **Large batches** (8 or more structurally changed files whose predicted
   merge costs more than 1.25 × a cold build, e.g. after a branch switch)
   are rebuilt from scratch instead.

JS/TS files are re-parsed one by one. A change to `ripple.yml` that affects
extraction (`ignore`, `ruby`, `js`, `boundary`) triggers a full rebuild;
other config changes (areas, rules, tests) only re-evaluate policy.

## Signals

Each signal loads in the background and is never on a query's path:

- **History** (`historyd.rs`): `git log` over the newest
  `history.max_commits` (20,000) non-merge commits on a low-priority
  thread, cached in a sidecar and updated incrementally when HEAD moves
  (debounced 500 ms, not during a rebase or `am`). Unavailable in a shallow
  clone or outside git.
- **Coverage** (`signals.rs`): the reports found (configured paths or the
  usual locations), re-parsed only when one appears or changes; marked stale when older
  than HEAD's commit or `coverage.max_age_days`.
- **Rank**: computed once history and coverage have settled and cached;
  `impact` never computes it, `file` and `rank` wait for it up to 5 s.

States (`ready`, `building`, `loading`, `unavailable`, `disabled`) are in
[METRICS.md](METRICS.md#signal-states).

## Files on disk

ripple writes nothing into the work tree.

| Path | What |
|---|---|
| `<socket dir>/<hash>.sock`, `.lock`, `.log` | The daemon's socket, its `flock` lock, its log. |
| `<git common dir>/ripple/history.bin` | History cache (`history-<worktree>.bin` for a linked worktree). |
| `<git common dir>/ripple/index-<hash>.snap` | The sleeping daemon's snapshot. |
| `.claude/`, `.codex/`, `.agents/skills/ripple/` | Only when you run `ripple skill install` (plus a `.ripple-skill` manifest). |

## Sleep

After `daemon.sleep_after_seconds` (300; `0` = never) with no request, no
file event and nothing pending, the daemon:

1. writes its current snapshot to `<git common dir>/ripple/index-<hash>.snap`
   (outside a git repository, next to the socket);
2. deletes older snapshots of other daemons in that directory that are at
   least 10 minutes old and either older than 3 days or beyond the newest
   3 (a daemon reads its own file only right after writing it, and a
   mapping survives unlink);
3. re-executes itself (`execve`, same pid), passing the listening socket
   and the lock to the new image in `RIPPLE_DAEMON_RESUME`. Memory freed
   inside a process mostly stays with the allocator, so a fresh image is
   the reliable way to give it back. Clients that connect meanwhile wait in
   the listen backlog;
4. the new image memory-maps the snapshot and answers from it. Its watcher
   starts after the old one stopped, so it first compares every workspace
   file and directory against the sleep time minus a 2 s margin.

While asleep, changed paths are checked against the mapped snapshot. A
change to an ignored or unindexed file is recorded and nothing else
happens. A relevant change wakes the daemon: a full index, as at start,
during which queries get the usual freshness treatment (answers marked
stale after `max_wait_ms`; `--sync` waits for the index).

If writing or reading back the snapshot fails, the daemon stays awake and
logs why. Re-execution failing falls back to sleeping in-process (dropping
the engine and serving from the mapping).

## Snapshot validation and atomic writes

The snapshot is a section file (`ripple-core/src/store.rs`): magic
`RPLSECT1`, format version, byte-order mark, a table of sections and a
checksum. Sections hold the graph's columns (paths, edges, both adjacency
arrays), method mentions, metrics, workspace membership as one bit per
node, the scope table and a JSON section with the small fields.

- **Key.** The snapshot records a key built from the format version, the
  ripple version, this binary's size and mtime, and the environment
  variables that change the pipeline. A rebuilt or different ripple, or a
  different environment, ignores the file and indexes from scratch.
- **Validation.** Loading checks the checksum, every invariant that queries
  rely on (for example that offsets and node ids are in range), and that
  the root and the effective config match. Any failure means "ignore and re-index".
- **Atomic writes.** The file is written to a temporary name and renamed
  over the old one: a reader sees the previous file or the complete new
  one, never a torn one.

## Memory on macOS

On macOS the daemon re-executes itself once at start with
`MallocSpaceEfficient=1` unless it is already set. The default allocator
keeps per-CPU magazines and freed large blocks; with this setting the
footprint on GitLab drops by about 40% (about 690 to about 400 MB awake)
at the same indexing speed, and a sleeping daemon stays small after
answering. When going to sleep without re-executing, ripple asks the
allocator to return free pages (`malloc_zone_pressure_relief` on macOS,
`malloc_trim` on Linux glibc).

Most of the remaining awake memory on a large Ruby repository is the
Rubydex resolver's graph. It is kept so that structural edits stay
incremental (hundreds of milliseconds on GitLab instead of a ~2.5 s
rebuild).
