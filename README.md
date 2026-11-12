# stud-finder, ripple edition

`ripple` is stud-finder rebuilt as a fast, always-fresh index for Rails +
React repositories. Point it at a change and it tells you what depends on
the changed code (Ruby constants and `require`, Rails conventions, JS/TS
imports, frontend HTTP calls matched to Rails routes), how risky each
changed file is (size, complexity, churn, coverage, hotspot rank) and which
rules fire, with the risk level and the actions they require. It is
omakase: no setup, no config file, sensible built-in areas and rules; the
first command indexes the repository and later ones answer in milliseconds.
Use it in CI, while you develop, to verify a change before you push, from
coding agents (CLI plus an agent skill), and in Unix pipelines. ripple
reports; you, your agent or your workflow decides.

**It is not** a complete list of dependents (about two thirds; it prints
the `rg -w` search that covers the rest), test selection, a bug predictor,
or a coverage tool. See [Limitations and evidence](#limitations-and-evidence).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/saugatkhadka/stud-finder/main/install.sh | bash
```

The script picks the release tarball for your OS and CPU, downloads it and
its SHA-256 file from GitHub releases, verifies the checksum and copies one
binary to `~/.local/bin`. It does not use sudo and changes nothing else; it
tells you if the directory is not on your `PATH`.

| Variable | Default | Effect |
|---|---|---|
| `RIPPLE_VERSION` | latest | Release tag to install, e.g. `ripple-v0.1.0`. |
| `RIPPLE_INSTALL_DIR` | `~/.local/bin` | Where to put the binary. |
| `RIPPLE_REPO` | `saugatkhadka/stud-finder` | Repository to download from. |
| `RIPPLE_NO_SKILL=1` | unset | Do not print the agent-skill hint. |

From source (Rust stable):

```sh
cargo install --git https://github.com/saugatkhadka/stud-finder --branch main ripple-cli
# or, in a checkout:
cargo build --release -p ripple-cli     # target/release/ripple
```

**Platforms.** Release binaries for macOS (arm64, x86_64) and Linux
(x86_64, arm64, glibc). The test suite runs in CI on macOS (arm64) and
Linux (x86_64); the x86_64 macOS and arm64 Linux binaries are cross-built
(arm64 Linux gets a smoke test). Most testing on real repositories was on
macOS. Unix only (Unix sockets, `flock`).

## 60-second tour

In Mastodon, with no `ripple.yml`. Change one comparison in
`Status#within_realtime_window?`, then:

```console
$ ripple impact
1 changed file; 138 direct dependents; 705 more at hops 2-3; 470 tests depend on it; method-level (845 if whole files changed); MEDIUM risk; may be incomplete (dynamic_dispatch)
INCOMPLETE app/models/status.rb: dynamic_dispatch: dispatches by computed name (`send`, `method_missing`, `eval`): methods reached that way are not in the graph
changed M app/models/status.rb  ruby  +1 -1  body within_realtime_window?  170 file dependents
  cx 160 (max 9) · churn 5/90d · hotspot #6
dependents 30/138:
  app/lib/activitypub/activity/announce.rb:42  ruby_constant Status  calls within_realtime_window?
  app/lib/activitypub/activity/create.rb:76  ruby_constant Status  calls within_realtime_window?
  app/workers/thread_resolve_worker.rb:21  ruby_constant Status  calls within_realtime_window?  [jobs]
  app/chewy/public_statuses_index.rb:55  ruby_constant Status
  …
  +108 more (--all)
further 705 files  hops 2:242 3:463  ruby:531 js:110 template:64  areas api:65 auth:7 config:5 jobs:65 …
  app/javascript/mastodon/actions/bookmarks.js  hop 2 via app/controllers/api/v1/bookmarks_controller.rb  inferred  crosses_boundary
  …
tests that depend on it 470: 32 direct
  spec/lib/activitypub/activity/announce_spec.rb:134
  …
risk MEDIUM  rules impacted_over_5pct, crosses_boundary
blind spots: methods matched by name; receivers untyped; JS/TS and send/try not checked; also run: rg -w 'within_realtime_window?'
not ready: coverage unavailable (no coverage report found in coverage/, tmp/coverage/, …)
```

The first call built the index (240 ms here) and started a daemon; the
callers come first, each with the line to read. One file, and the riskiest
files overall:

```console
$ ripple file app/models/status.rb
app/models/status.rb
  ruby
  362 code lines, 57 methods · cx 160 (max 9) · churn 5/90d · hotspot #6
  churn: 281 commits in the history window, 5 in the last 90 days, last 245 days ago
  hotspot: rank 6 of 2734 (percentile 99.8), a hotspot
  most complex methods:
       9  set_conversation (lines 445-456)
       …
  dependents: 138, most depended-on: app/models/tag.rb (59), app/models/media_attachment.rb (42), …
  tests depending on it directly: 32

$ ripple rank --limit 2
2 of 2734 ranked files; hotspots: top 1% (rank <= 28)
1*  98.8  app/lib/activitypub/tag_manager.rb
       churn 5/90d, 44 total (top 2%) · cx 123 / max method 17 (top 2%) · 124 non-test dependents (top 1%)
2*  98.5  app/models/account.rb
       churn 6/90d, 329 total (top 1%) · cx 128 / max method 10 (top 5%) · 268 non-test dependents (top 1%)
```

And what is ready, and how to fix what is not:

```console
$ ripple doctor
config   ok: no ripple.yml (built-in defaults); 8 areas (8 built-in), 17 rules (17 built-in)
daemon   running (pid 63175, ripple 0.1.0, up 528 s)
index    5734 files, 13635 edges, built 12 s ago, fresh
signal   complexity ready
signal   churn      ready
signal   cochange   ready
signal   coverage   unavailable (no coverage report found in coverage/, …)
signal   hotspot    ready
skill    …/mastodon/.claude/skills/ripple/SKILL.md (project): not installed
nothing to fix
optional:
  - coverage: … (run your test suite with SimpleCov or Jest/Vitest coverage, or set coverage.paths)
  - agents: `ripple skill install` (project) or `ripple skill install --user`
```

## For coding agents

The binary embeds an agent skill: when to call ripple, how to read the
report, and what it cannot see.

| Command | Effect |
|---|---|
| `ripple skill install` | Write the skill for this repository: `.claude/skills/ripple/SKILL.md` (plus a `.ripple-skill` manifest). |
| `--user` | Under `$HOME` instead of the repository. |
| `--agent claude\|codex\|agents` (repeatable) | Which agents' directories: `.claude/`, `.codex/` or `.agents/skills/ripple/` (default `claude`). |
| `ripple skill update` | Replace an installed skill with this binary's version. |
| `ripple skill uninstall` | Remove it. |
| `ripple skill status` | Installed, current, outdated or edited (exit 1 if outdated or edited). |
| `ripple skill print` | Print it to stdout (paste it into any agent's instructions). |

`update` and `uninstall` refuse to touch a skill you edited (exit 2) unless
you pass `--force`. `ripple doctor` reports the skill's state.

If your agent does not load skills, put this in its instructions:

```
Before editing a Ruby method or controller action: ripple dependents FILE --scope NAME.
After editing: ripple impact --sync (working tree vs HEAD; --base main for a branch).
Read every "dependents" line that calls the changed method and check it still fits.
Run the "also run: rg -w …" search ripple prints; it covers what ripple cannot see.
Treat "further", "tests" and "usually changes with" as context, not a to-do list.
INCOMPLETE or "not ready:" means the answer is missing something; ripple doctor says why.
--format json gives structure; ripple schema impact describes it.
```

## For CI

ripple reports; your workflow decides. It computes the impact, the fired
rules, the risk level and the required actions (`human_review`,
`run_full_suite`, `add_tests`, `check_cochange_partners`, or yours); the
workflow fails the job, labels, comments or requests a reviewer.

```sh
ripple impact --base origin/main --format github --fail-on high
ripple impact --base origin/main --fail-on human_review,run_full_suite
```

- `--fail-on` takes levels (`low` … `critical`), action names, or `never`.
- Exit codes: **0** ran, nothing reached `--fail-on`; **1** the risk or a
  required action reached it (the report is complete); **2** ripple could
  not tell you (usage, config, git or internal error).
- `--format github`: annotations on changed files, the markdown report in
  the job summary, and step outputs `risk`, `require`, `impacted`, `tests`,
  …. `--format markdown`: a PR comment body.
- Under `CI` or `GITHUB_ACTIONS` no daemon is started; ripple indexes in
  the process. Check out with `fetch-depth: 0`.

Copy [`examples/github-actions.yml`](examples/github-actions.yml) (sticky
comment and a `risk:<level>` label) or
[`examples/github-workflow.yml`](examples/github-workflow.yml) (`ripple
check`, JSON artifact). Details: [docs/CI.md](docs/CI.md).

## Unix use

- **stdin.** `-` as the only path reads paths, one per line:
  `git diff --name-only main | ripple impact -`;
  `git ls-files app/models | ripple file -` writes one JSON object per line.
- **JSON everywhere.** `--format json` on `impact`, `dependents`, `check`,
  `file`, `rank`, `config show`. `ripple schema impact|file|rank` prints
  the JSON Schema (copies in [`docs/`](docs/)). Optional fields are omitted,
  never `null`.
- **Streams.** Data on stdout; diagnostics, warnings and hints on stderr
  (`-q` drops hints). Colour only on a terminal, never with `NO_COLOR` or
  `TERM=dumb`.
- **Completions.** `ripple completions bash|zsh|fish|elvish|powershell`.

```sh
ripple impact --base main --format json | jq -r '.policy.risk'
```

## Commands

Global flags: `--root DIR` (`RIPPLE_ROOT`; default: the enclosing git work
tree), `--config PATH` (`RIPPLE_CONFIG`; its own daemon), `--no-daemon`.

| Command | What it does |
|---|---|
| `impact [FILES]` | The report for a change. No arguments: working tree vs HEAD, untracked files included. `--base REF`: since `merge-base(REF, HEAD)`. `--staged`: index vs HEAD. `--files A,B` or `FILES`: these files as wholly changed. `--scope NAME` (one file): only that method or action. |
| `dependents FILE` | The report of FILE as if it had changed; `--scope NAME` (repeatable) starts from those methods or actions. |
| `file FILE` | One file's profile: lines, complexity, top methods, churn, coverage, dependents, tests, co-change, hotspot rank, signal states. |
| `rank` | Riskiest non-test Ruby and JS/TS files. `--limit N`, `--path GLOB`, `--area NAME`, `--by score\|churn\|complexity\|coverage`. Exit 4 with no ranking yet. |
| `check --base REF` | CI entry point; `--markdown-out`, `--json-out`; fail-on default from `ci.fail_on`. |
| `doctor` (`status`) | What is ready and what to run if not. Exit 1 when something needs fixing. |
| `config show` / `config check` | The configuration in effect (built-in and yours, with origins and this repository's cut-offs) / validate `ripple.yml`. |
| `init [--stdout] [--force]` | Write a `ripple.yml` with every default spelled out, commented. |
| `skill …` | See [For coding agents](#for-coding-agents). |
| `history build\|status` | Build or show the git-history cache. |
| `daemon status\|start\|stop` | Manage the daemon (exit 3 from `status` when none runs). |
| `schema`, `completions` | JSON Schemas; shell completions. |
| `why FROM TO`, `dependencies FILE`, `areas FILE`, `stats` | Shortest path between two files; what a file uses; how a file is classified; index statistics. |

Shared by `impact` and `dependents`: `--format`, `--depth N` (default 3),
`--min-confidence exact|inferred|heuristic`, `--all` / `--limit N`,
`--sync` (wait until every edit made before the command is indexed),
`--no-wait` (answer at once, possibly marked stale), `--file-level`.

Exit codes: 0 ok; 1 `--fail-on` reached, `doctor` found something to fix,
or `skill status` found an outdated or edited skill; 2 error; 3 no daemon
(`daemon status`); 4 no ranking yet (`rank`).

## Reading the report

The summary line counts everything; then `changed` (each file, what
changed in it, a metrics line), `dependents` (one hop, callers first, with
the line to read), `also mentions` (same name, receiver unknown),
`further` (hops 2–3, summarised), `tests`, `usually changes with`, `risk`,
`blind spots` and `not ready:`. Three or more plain non-source files of one
directory (locale JSON, fixtures) collapse to one `changed` line; the JSON
keeps every file. Each list is cut to a budget and ends with
`+N more (--all)`; counts are never cut.

Section by section: [docs/METRICS.md](docs/METRICS.md#the-report-section-by-section).

## Metrics

`cx 160 (max 9) · churn 5/90d · cov 62% (changed lines 20%) · hotspot #6`:
cyclomatic complexity of the file and its most complex method, commits in
the last 90 days of history, line coverage from your existing reports
(unknown is not zero), and the hotspot rank (top 1%). The rank weighs
churn 3, complexity 1, dependents 0.5 and uncovered lines 1, each as a
percentile. Definitions, formula and signal states:
[docs/METRICS.md](docs/METRICS.md).

## Configuration

**Zero config.** With no `ripple.yml`, ripple detects the test directories,
CODEOWNERS and coverage reports in the usual places, applies 8 built-in
areas (payments, auth, migrations, api, config, jobs, mailers, shared
components, each only where its paths exist) and 17 built-in `default.*`
rules, and computes relative thresholds from this repository.

**Your `ripple.yml`** (or `.ripple.yml`) extends the defaults: your areas
and rules are added; one with a built-in's name or id replaces it;
`defaults: {disable: [...]}` drops single built-ins and
`defaults: {areas: false}` / `{rules: false}` drops them all. Rule
conditions can be absolute (`complexity_over: 200`) or relative to this
repository (`complexity_top_percent: 5`, `dependents_top_percent`,
`max_method_complexity_top_percent`, `impacted_files_over_percent`).

```yaml
rules:
  - id: payments_needs_review
    when: { touches: [payments], impacted_files_over: 20 }
    risk: high
    require: [human_review]
daemon:
  sleep_after_seconds: 300   # 0 = never sleep
```

- `ripple config show` prints what is in effect and where each piece came
  from, with this repository's percentile cut-offs.
- `ripple init` writes a commented `ripple.yml` with every default and key
  spelled out (it changes nothing until you edit it).

All keys, `when` conditions, the built-in rules and their calibration:
[docs/CONFIG.md](docs/CONFIG.md).

## Daemon, memory and sleep

The first command starts one daemon per repository (and `--config`); later
commands talk to it over a Unix socket. It watches the work tree and
re-indexes edits incrementally. Before answering it makes sure it has seen
every edit made before the request and waits up to 150 ms
(`daemon.max_wait_ms`) for them to be indexed; otherwise it answers and
marks the answer stale. `--sync` always waits.

After 5 minutes without requests or edits (`daemon.sleep_after_seconds`)
it writes its index to `<git dir>/ripple/index-<hash>.snap`, re-executes
itself and serves from the memory-mapped file: a few MB instead of
hundreds. An edit to an indexed file wakes it with a full re-index. After
30 minutes without a request it exits (`daemon.idle_timeout_minutes`).
ripple writes nothing into your work tree. How it works:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Performance

Measured by the owner on an Apple-silicon laptop that was under other
load at the time; release build, daemon mode. Numbers marked † were
reported by an agent run and are noisier.

| | Mastodon | Gumroad | GitLab |
|---|---:|---:|---:|
| Files in git | 9,730 | 10,190 | 65,492 |
| First call, including the cold index | ~240 ms | ~450–740 ms | ~2.5 s |
| Warm `impact`, whole process, p50 | ~4.6 ms | ~4.1 ms | ~10 ms small diff; ~25–40 ms 30-file diff † |
| Save a method-body edit → fresh answer, p50 | 33 ms | 41 ms | 136 ms |
| Save an edit that changes a constant reference or class structure → fresh answer | ~35 ms | ~71 ms | ~430 ms |
| Daemon memory, awake | 43 MB (was 86) | ~55 MB † (was 138) | 407 MB (was 713) |
| Daemon memory, asleep | 6 MB | ~5 MB † | 18 MB (27 MB after answering a query while asleep) |
| First query after sleep, nothing edited | 8 ms | — | 11 ms |
| First query after an edit made while asleep (full re-index), `--sync` | ~0.2 s † | ~0.35 s † | ~2.3 s † |
| Snapshot on disk | 2.6 MB | 4.4 MB | 27.7 MB |

After sleep the answer is the same as awake. Without `--sync`, a query
right after an edit made while asleep answers in about 180 ms, marked
stale. Asleep, ripple is at or near the ~10 MB we aimed for. Awake on a
GitLab-sized repository it still uses about 400 MB, about half of it the
Rubydex resolver's graph, which is kept so that edits stay incremental.

## Limitations and evidence

We measured ripple against real history. `crates/ripple-eval` reproduces
the dependents numbers (`ripple-eval breaks`); the rank and rule numbers
come from replay scripts kept outside this repository.

**Dependents are not complete.** Oracle: commits that removed or renamed a
Ruby method or constant where other files of the same commit had to change
(GitLab 344 cases, Gumroad 114, Mastodon 81).

| Answer | Found at 1 hop | Within 3 hops |
|---|---:|---:|
| ripple, method-level | 64–67% | 75–81% |
| ripple, whole file | 64–67% | 75–83% |
| `rg -w NAME` | 100% | — |

ripple misses templates and JS/TS that use a Ruby name with an unknown
receiver, dynamic calls and some unresolved constants. A word search finds
everything that mentions the name, with much more noise; ripple prints the
search to run.

**No measured benefit for agents.** Six edit tasks on Gumroad: identical
diffs with and without ripple on 5 of 6. Review of 12 real commits later
followed by a fix (plus 3 controls): 5 hits with ripple, 5 without; in 7 of
the 12, the file the fix touched was not in ripple's output at all (mostly
code following the same pattern with no dependency edge). We do not claim
ripple improves agent edits or reviews.

**Rank is roughly on par with churn alone.** Both are far better than
random (of the files later fixes touched, 50–52% were in the top 10% on
Gumroad and 38–40% on Mastodon, against 10% for random), but the weighted score is not
clearly better than ranking by recent churn.

**Built-in rules flag review effort, not bugs.** Replayed over each
repository's history, the built-in rules flag HIGH or above on 8.3% of
Mastodon and 16.9% of Gumroad commits. A fix commit followed a flagged
commit within 30 days 20% of the time vs 8% for unflagged ones (Mastodon),
and 65% vs 24% (Gumroad). Two repositories; yours will differ.
Method and the replay: [docs/CONFIG.md](docs/CONFIG.md#calibration).

**Other limits.**

- Co-change is context: in the review trial it never named a file that a
  later fix touched.
- Coverage has only been tested against synthetic reports.
- No test selection: `tests` is neither a minimal nor a sufficient set.
- Static analysis only: `send`, `method_missing`, string-built constants,
  dynamic imports, hash and JSON keys are invisible; `INCOMPLETE` lines
  mark the worst cases. JS/TS changes are whole-file.
- On histories dominated by bot or sync commits, churn and co-change say
  little (co-change turns itself off).
- A cold index (CI) cannot list the dependents of a deleted Ruby file; a
  running daemon keeps them. Git submodules are not indexed.

## ripple and the stud-finder gem

stud-finder started as a Ruby gem that scores every file in one batch run
(fan-in, fan-out, complexity, churn, coverage, temporal coupling) and
prints a ranked table. ripple replaces it on `main`.

| | stud-finder gem | ripple |
|---|---|---|
| Ranked list | Ruby and JS/TS tables | `ripple rank`: one list, a reason per row |
| Weights | Fixed, over fan-in, fan-out, complexity, churn, coupling | Churn-heavy (3/1/0.5/1), chosen by replay |
| Diff impact, dependents, rules, CI | No | Yes |
| Running | One batch run | Daemon; milliseconds per query, fresh while you edit |
| Install | Ruby gem | One static binary |

On Gumroad the two tools' top 30 Ruby files share 13. We have not
measured which list predicts fixes better.

**The gem** stays available at tag `v0.7.0`:

```sh
git clone --branch v0.7.0 https://github.com/saugatkhadka/stud-finder
```

Its README at that tag explains how to build and run it.

ripple releases are tagged `ripple-vX.Y.Z`. Changes: [CHANGELOG.md](CHANGELOG.md).

## Environment

| Variable | Effect |
|---|---|
| `RIPPLE_ROOT`, `RIPPLE_CONFIG` | Same as `--root`, `--config`. |
| `RIPPLE_NO_DAEMON=1` | No daemon autostart (also under `CI` or `GITHUB_ACTIONS`). |
| `RIPPLE_SOCKET_DIR` | Directory for the daemon's socket, lock and log. |
| `NO_COLOR` | Plain text. |
| `RIPPLE_DEBUG` | Daemon debug log. |

## License

MIT. See [LICENSE](LICENSE).
