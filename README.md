# ripple

ripple is a fast, per-repository index for Rails + React codebases. Given a
diff (or a file), it answers three questions:

1. **What depends on the changed code?** Files that reference it through Ruby
   constants and `require`, Rails conventions (associations, views, routes to
   controllers), JS/TS imports, and frontend HTTP calls matched to Rails
   routes.
2. **How risky is each changed file?** Its size, complexity, churn (how often
   it changed recently), test coverage (from reports you already have) and
   its hotspot rank.
3. **Which of your `ripple.yml` rules fire?** Each fired rule gives a reason
   that names the files and numbers, a risk level and the actions it
   requires (for example `human_review`).

It is meant for CI, for people reviewing a change, and for coding agents,
through the `ripple` CLI and an agent skill (`skill/ripple/SKILL.md`).
`ripple rank` replaces stud-finder's ranked hotspot list.

**What ripple is not:**

- **Not a complete list of dependents.** It is static analysis. Against
  real removals and renames it found about two thirds of the files that had
  to change (see [Limitations and evidence](#limitations-and-evidence)). A
  word search (`rg -w NAME`) finds more. ripple prints the search to run.
- **Not test selection.** The `tests` section lists tests that depend on
  the changed code. It is neither a minimal nor a sufficient set to run.
- **Not a bug predictor.** Rules and ranks point at files worth a careful
  look. We could not show that they do better than recent churn alone.
- **Not a coverage tool.** It reads existing coverage reports. It never runs
  tests.

## Quick start

```sh
cargo build --release -p ripple-cli     # binary: target/release/ripple
cd your-rails-app
ripple init                             # writes a commented starter ripple.yml
ripple config check                     # validates it; shows what each area matches
ripple impact                           # working tree vs HEAD, untracked files included
ripple impact --base main               # everything since merge-base(main, HEAD)
ripple dependents app/models/purchase.rb --scope refund!   # what uses Purchase#refund!
ripple file app/models/purchase.rb      # one file's profile
ripple rank                             # riskiest files first
```

The first command starts a background daemon for the repository (one per
repository and config file). Later commands reuse it. CI runs without a
daemon: see [`examples/github-workflow.yml`](examples/github-workflow.yml)
and `ripple check`.

Without a `ripple.yml`, ripple still reports dependents and metrics, but
shows no risk section. Run `ripple init` to get one.

## Commands

Every command takes the global flags `--root DIR` (`RIPPLE_ROOT`; default:
the enclosing git work tree), `--config PATH` (`RIPPLE_CONFIG`; use this
file instead of `<root>/ripple.yml`, served by its own daemon) and
`--no-daemon` (index in this process; slower, no freshness wait). Paths may
be relative to the current directory or absolute.

| Command | What it does |
|---|---|
| `impact [FILES]` | The report for a change. With no arguments: working tree vs HEAD plus untracked files. `--base REF`: since `merge-base(REF, HEAD)`, including uncommitted and untracked files. `--staged`: index vs HEAD. `--files A,B` or `FILES`: these files as wholly changed. `--scope NAME` (with one file): only that method or controller action changed. |
| `dependents FILE` | The `impact` report of FILE as if it had changed. `--scope NAME` (repeatable) starts from those methods or actions. |
| `file FILE` | One file's profile: language, areas, code lines, complexity, its 5 most complex methods, churn, coverage, dependents, dependencies, direct tests, co-change partners, hotspot rank, and the state of each signal. Waits up to 5 s for history, coverage and the ranking on a fresh daemon (`--no-wait`: answer at once). |
| `rank` | Riskiest non-test Ruby and JS/TS files first, each with the reason. `--limit N` (20), `--path GLOB` (repeatable), `--area NAME`, `--by score\|churn\|complexity\|coverage`. Waits up to 5 s for history and coverage (`--no-wait`: at once); with no ranking yet it prints `ranking not ready: …` and exits 4. |
| `check --base REF` | CI entry point: `impact` of the branch, always with the risk section. `--format github\|markdown\|json\|text` (github under Actions), `--fail-on low\|medium\|high\|critical\|never` (default `ci.fail_on`), `--markdown-out PATH`, `--json-out PATH`. Never starts a daemon. |
| `history build` / `history status` | Build or update the git-history cache now; show its state. |
| `schema` | Print the JSON Schema of the report (see [JSON output](#json-output)). |
| `daemon status\|start\|stop` | Manage the daemon. `serve [--foreground]` runs it directly. |
| `init [--force]` | Write a commented starter `ripple.yml`. `config check` validates it. |
| `why FROM TO`, `dependencies FILE`, `areas FILE`, `stats` | Shortest dependency path between two files; what a file depends on; how `ripple.yml` classifies a file; index statistics. |

Flags shared by `impact` and `dependents`:

| Flag | Meaning |
|---|---|
| `--format text\|json` (`impact` also `markdown`) | Output format. Default text. |
| `--depth N` | Hops to follow from a changed file (`0` = unlimited; default `query.max_depth`, 3). |
| `--min-confidence exact\|inferred\|heuristic` | Ignore weaker edges. |
| `--all` / `--limit N` | Do not cut the lists / cut each list at N (`changed` keeps at least its default 30). Counts are never cut. |
| `--sync` | Wait until every file edit made before the command is indexed, however long that takes. |
| `--no-wait` | Answer from the current index at once (the answer may be marked stale). |
| `--file-level` (`impact`) | Treat each changed file as wholly changed (the wider answer). |

**Exit codes:** 0 success; 1 `check` found risk at or above the fail-on
level; 2 usage, config, git or internal error, or a file not in the index;
3 `daemon status` found no daemon; 4 `rank` has no ranking yet.

## How to read the report

Example (`ripple dependents app/helpers/currency_helper.rb --scope string_to_price_cents`
on Gumroad, shortened):

```
1 changed file; 4 direct dependents (+55 only inherit/include); 1 more mention the name with no edge; 1304 more at hops 2-3; 1457 tests depend on it; method-level (1410 if whole files changed); MEDIUM risk
changed M app/helpers/currency_helper.rb  ruby  body string_to_price_cents  68 file dependents
  cx 44 (max 5) · churn 0/90d
dependents 4/4:
  app/models/purchase.rb:3415  ruby_constant inherits CurrencyHelper  calls string_to_price_cents  [payments]
  app/models/link.rb:737  ruby_constant inherits CurrencyHelper  calls string_to_price_cents
  …
inherits/includes, does not call it 55: app/business/payments/… +45 more (--all)
also mentions `string_to_price_cents` (no dependency edge; receiver type unknown) 1 file, defined only here:
  app/modules/base_price/shared.rb:14
further 1304 files  hops 2:626 3:678  ruby:1041 js:144 template:119  areas auth:3 payments:28  notable 184
  app/business/payments/charging/charge_processor.rb  hop 2 via …/paypal_charge_processor.rb  high_tier  [payments]
  +1294 more (--all)
tests that depend on it 1457: 1 direct
  spec/helpers/currency_helper_spec.rb:41
  +1456 more (--all)
risk MEDIUM  rules impacted_over_200, reaches_critical_direct, crosses_boundary  owners @payments @security @web
blind spots: methods matched by name; receivers untyped; JS/TS and send/try not checked; also run: rg -w string_to_price_cents
not ready: coverage unavailable (no coverage report found in coverage/ …)
```

Section by section, in the order they appear:

- **Summary line.** Counts of each section below, whether the answer is
  method-level, and the risk. "Impacted" means dependents at any depth,
  excluding tests and the changed files.
- **`REMOVED … still mentioned`** (`removed_methods`). Methods the diff
  deleted or renamed that some file still mentions by name. These are the
  most likely breakages.
- **`INCOMPLETE`** (`incomplete`). Reasons the lists may be missing files:
  `global_file` (a file like `Gemfile.lock` or an initializer that anything
  may depend on), `not_in_graph`, `dynamic_dispatch` (the file uses `send`,
  `method_missing` or `eval`), `unresolved_references`, `stale_index`.
- **`changed`.** One line per changed file: status, language, what changed
  (`body m` = only method `m`'s body, `added m`, `signature`, `class_body`,
  or `whole file` with the reason), and its file-level dependent count. Under
  each source file, one metrics line (see [Metrics](#metrics)).
- **`dependents`.** Files one hop away, each with the line of the reference,
  the edge kind and its confidence. For a method-level change, files that
  call the changed method come first, with the line of the call. **Read each
  call line and ask whether the call still fits the new behaviour.**
- **`inherits/includes, does not call it`** (`inherit_only`). Subclasses and
  includers that never mention the changed methods. Ruby could still
  dispatch to them, so they stay in the impact.
- **`also mentions NAME`** (`also_mentioned`). Files that mention the
  changed method's name but have no dependency edge, because ripple cannot
  tell the receiver's type (`purchase.charge.total`, views, mailers). Listed
  only when the name is rare; otherwise only counted. For a rename, removal
  or signature change, treat these like dependents.
- **`relies on`** (`relies_on`). What the changed code itself uses (one hop
  forward).
- **`further`.** Files 2–3 hops away, as counts by depth, language and area,
  plus up to 10 notable ones (high-tier area, crossing the Ruby/JS boundary,
  inferred edge). Context, not a to-do list.
- **`tests that depend on it`** (`tests`). Tests that reach the change
  through the graph. Hop-1 tests are listed with the line that mentions the
  method; the rest are counted. This is not coverage and not a list of tests
  to run.
- **`usually changes with (not in the diff)`** (`usually_changes_with`). Up
  to 5 files outside the diff and its direct dependents that git history
  shows changing with a changed file. Context only (see
  [co-change](#co-change-usually-changes-with)).
- **`risk`** (`policy`). The highest risk among the fired rules, the rule
  ids, required actions and suggested owners. With `--format json` or
  `check`, each rule has a message naming the files and numbers. Present when
  `ripple.yml` defines rules or areas and a rule fired, and always in
  `check`.
- **`warning:`** lines. For example `global_file_changed`,
  `changed_file_not_indexed`, `no_line_counts` (a rule needs diff line
  counts and there is no diff), `rule_signal_not_ready`.
- **`blind spots`.** For method-level answers: what ripple cannot see, and
  the `rg -w` search that covers it. Run it.
- **`not ready:`** Signals that are still building or unavailable (see
  [signal states](#signal-states)).

Each list is cut to a small budget (30 dependents, 10 tests, 10 further
files, …) and ends with `+N more (--all)`. The counts in the summary are
not cut.

## Metrics

Metrics are computed for changed source files (not tests, not deleted, not
"other" files such as JSON or YAML). The text form is one line:
`cx 433 (max 88) · churn 41/90d · cov 62% (changed lines 20%) · hotspot #3`.
A part is left out when it is unknown or its signal is not ready.

### Lines

- **What:** code lines: lines with something other than whitespace or a
  comment (templates: non-blank lines).
- **Why:** size. Big files take longer to review and hide more.
- **Read it:** as context for complexity. A 3,000-line model with complexity
  1,200 is not unusual; a 100-line file with complexity 80 is dense.

### Complexity and max method complexity

- **What:** cyclomatic-style complexity. Each method scores `1 + decisions`
  (`if`, `unless`, `case`/`when`, loops, `&&`/`||`, `&.`, `rescue`, iterator
  blocks; for JS/TS, ESLint's "classic" rules). Ruby aims to match RuboCop's
  `Metrics/CyclomaticComplexity`. **Complexity** (`cx`) is the file's sum
  of method scores plus decisions outside methods. **Max method** (`max`) is
  its most complex method.
- **Why:** more paths through the code means more cases a change can break
  and a test can miss.
- **Read it:** `cx` grows with file size; `max` tells you whether there is
  one knot. `ripple file` lists the 5 most complex methods with line ranges.
  A rule like `max_method_complexity_over: 25` with `changed_lines_over: 10`
  flags an edit to a file with a hard method in it.

### Churn (total and last 90 days)

- **What:** the number of commits that touched the file. `churn 41/90d` is
  commits in the 90 days **before the newest commit in the history window**
  (not before today: a repository with no commits this quarter still shows
  its last quarter's activity). The total counts commits in the whole window
  (the newest 20,000 commits by default, `history.max_commits`).
- **Why:** code that changed often recently tends to change again, often to
  fix something. In our replay, ranking by recent churn alone did about as
  well as any weighted combination we tried at predicting which files later
  fix commits touched.
- **Read it:** high recent churn means the area is in motion: check for
  concurrent work and recent fixes. Churn includes sweeps (mass renames,
  formatting), so look at `git log` before drawing conclusions.

### Coverage (percent, changed-lines coverage, stale, unknown)

- **What:** from existing reports under `coverage/` (SimpleCov
  `.resultset.json`, `lcov.info`, `lcov/*.lcov`, Istanbul
  `coverage-final.json`; set `coverage.paths` for others). `cov 62%` is line
  coverage of the file. `changed lines 20%` is coverage of the lines this
  diff added or changed (needs a diff). `stale` means the report is older
  than HEAD's commit or than `coverage.max_age_days`; line numbers may have
  moved, so read it as approximate.
- **Unknown is not zero.** A file in no report has no coverage value at all
  (no `cov` part, no `coverage` field). It does not count as 0% in rules or
  in the rank, unless a rule opts in with `uncovered_includes_unknown: true`.
- **Why:** low coverage on changed lines means the change may ship untested.
- **Read it:** compare `changed lines` with the file's percentage. Coverage
  has so far been exercised only against synthetic reports; treat it as
  unproven on your reports and check a few files by hand.

### Hotspot rank, percentile, formula and weights

- **What:** `ripple rank` scores every non-test Ruby and JS/TS file. Each
  factor is turned into a percentile (the share of ranked files with a
  strictly smaller value), so one huge value cannot dominate:

  ```
  score      = 100 * (3*churn + 1*complexity + 0.5*dependents + 1*uncovered) / (sum of the weights used)
  churn      = percentile of (commits in the last 90 days, then total commits)
  complexity = mean(percentile of file complexity, percentile of max method complexity)
  dependents = percentile of direct non-test dependents
  uncovered  = 1 - line coverage, only for files in a coverage report (else its weight is dropped;
               churn's too when git history is unavailable)
  hotspot    = rank <= ceil(files * rank.hotspot_percent / 100)    (top 1% by default)
  ```

  `rank --format json` prints this formula with the weights in effect.
  `rank` is the position (1 = riskiest), `of` the number of ranked files,
  `percentile` the share of files with a lower score. `hotspot #3` in the
  metrics line appears only for hotspots; `ripple file` shows any file's
  rank.
- **Weights:** `rank.weights` in `ripple.yml` (defaults `churn: 3,
  complexity: 1, dependents: 0.5, uncovered: 1`). Chosen by replaying ~300
  commits each of Gumroad and Mastodon; the result is roughly on par with
  ranking by churn alone (see [the table](#rank-vs-later-fix-commits)).
- **Read it:** a hotspot is a file to review slowly, not a file that is
  wrong.

### Dependents counts

- **What:** in `changed`, the file-level count of direct dependents (fan-in).
  In the summary: direct dependents of the changed methods (method-level),
  those that only inherit or include, mentions with no edge, files further
  away, and tests. `impacted_file_level` in the JSON is what the impact
  would be if each changed file were wholly changed.
- **Why:** a rough blast radius.
- **Read it:** a count of the references ripple found, not a complete count
  (see [Limitations](#limitations-and-evidence)). In a dense Rails app, 2–3
  hops reach a large part of the app, so read the `further` count as a rough
  sketch, not a to-do list.

### Co-change ("usually changes with")

- **What:** files that changed in the same commits as a changed file, from
  git history: `with X in 50 of its 63 commits (79%)`. Commits touching more
  than 30 files are ignored as sweeps. Files already in the diff or directly
  connected in the graph are left out.
- **Why:** a usual partner missing from the diff can be a forgotten edit
  (a locale file, a serializer, a spec).
- **Read it:** as context. In the review trial below it never named a file
  that a later fix touched. Co-change turns itself off when history is short
  (under 300 commits) or mostly sweeps (over 25% of commits); see
  [Limitations](#limitations-and-evidence).

### Signal states

Each signal (`complexity`, `churn`, `cochange`, `coverage`, `hotspot`) has a
state, shown in `signals` (JSON) and on the `not ready:` line:

| State | Meaning |
|---|---|
| `ready` | Data is loaded and current. |
| `building` / `loading` | Being computed in the background (git history on a fresh daemon, coverage reports being read, the ranking waiting for them). Answers do not wait for it. |
| `unavailable` | Cannot be computed; the reason says why (no coverage report, shallow clone, not a git repository, co-change off because of sweeps). |
| `disabled` | Turned off in `ripple.yml` (`history.enabled: false`, `coverage.enabled: false`). |

A rule that needs a signal that is not ready does not fire and adds one
`rule_signal_not_ready` warning. The built-in metric rules skip quietly
while signals load (but, like any rule using `changed_lines_over`, they add
a `no_line_counts` warning when there is no diff, for example in
`dependents`).

## `ripple.yml`

A missing file means no areas and the built-in rules (see below). Unknown
keys and invalid values are errors that name the key and line.
`ripple config check` validates the file and shows what each area matches.
[`examples/ripple.yml`](examples/ripple.yml) uses every `when` key.

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Must be 1. |
| `ignore` | node_modules, vendor, tmp, log, public, coverage, dist, build | Globs never analysed, in addition to `.gitignore`. |
| `areas.<name>` | — | `paths` (globs), `tier` (`low`/`medium`/`high`/`critical`, required), `owners`, `description`. |
| `global_files` | initializers, `application.rb`, environments, `boot.rb`, Gemfile(.lock), package.json, lockfiles | Files anything may depend on in ways a static graph cannot see. |
| `tests` | `spec/**`, `test/**`, `__tests__`, `*.test.*`, `*.spec.*`, `*_spec.rb`, `*_test.rb`, `*.stories.*` | Test files: listed under "tests that depend on it", never counted as impacted. |
| `rules[]` | built-in rules | `id`, `when` (`{}` always fires), `risk`, `require: [..]`, `description`. The change's risk is the highest risk among fired rules (low if none); `require` lists are merged. |
| `query.max_depth` | `3` | Hops to follow; `0` = unlimited. |
| `query.min_confidence` | `heuristic` | Ignore edges below this. |
| `ci.fail_on` / `ci.github` / `ci.max_listed` | none / `true` / `50` | `check` exits 1 at or above this risk; write `$GITHUB_STEP_SUMMARY`; files listed in markdown. |
| `daemon.max_wait_ms` | `150` | How long a query waits for pending edits (see [Freshness](#freshness)). |
| `daemon.idle_timeout_minutes` | `30` | The daemon exits when idle this long; `0` = never. |
| `history` | — | `enabled` (`true`), `max_commits` (`20000`; `0` = all), `max_age_days` (none), `max_files_per_commit` (`30`), `min_together` (`3`), `min_confidence` (`0.3`), `min_lift` (`2.0`), `limit` (`5`). |
| `coverage` | — | `enabled` (`true`), `paths` (`[]` = auto-detect under `coverage/`), `max_age_days` (none). |
| `rank.hotspot_percent` | `1` | Top share of `ripple rank` that counts as a hotspot. |
| `rank.weights` | `churn: 3, complexity: 1, dependents: 0.5, uncovered: 1` | Rank weights (each ≥ 0; `complexity` or `dependents` above 0). |
| `ruby`, `js`, `boundary` | — | Extractor settings (autoload roots, tsconfig, route files, HTTP callees). Changing them rebuilds the index. |

Globs follow gitignore rules: `*` stays in one directory, `**` crosses
directories, a pattern without `/` matches at any depth, a leading `/`
anchors to the root, a directory matches everything under it. In `touches`
and `reaches`, a name that is an area means that area; anything with
`/ * ? [ { .` is a glob.

### `when` keys

All keys in a rule must hold for it to fire.

| Key | Holds when |
|---|---|
| `touches: [area\|glob]` | A changed file is in a listed area or matches a glob (old path of a rename too). |
| `touches_tier: T` | A changed file is in an area of tier T or higher. |
| `reaches: [area\|glob]` | An impacted file (not a test, not changed) is in a listed area or glob. |
| `reaches_tier: T` | The same, for an area of tier T or higher. OR-ed with `reaches`. |
| `within: N` | Limits `reaches`/`reaches_tier` to N hops. |
| `min_confidence: C` | For this rule, ignore edges below C. |
| `impacted_files_over: N` | More than N impacted non-test files. |
| `changed_files_over: N` | More than N changed files. |
| `changed_lines_over: N` | More than N added + deleted lines (needs a diff). Next to a per-file key: lines of that same file. |
| `crosses_boundary: true\|false` | An impacted file is on the other side of the Ruby/JS boundary. |
| `touches_global: true\|false` | A changed file matches `global_files`. |
| `untested: true\|false` | A changed source file has no test among its dependents. |
| `missing_cochange: true\|false` | A file that changed with a changed file in at least `cochange_min_confidence` (default `0.5`) of its commits is not in the diff. |
| `churn_over: N` | A changed source file had more than N commits in the last 90 days of history. |
| `complexity_over: N` | A changed source file's complexity is above N. |
| `max_method_complexity_over: N` | A changed source file has a method above N. |
| `coverage_under: P` | A changed source file's line coverage is below P% (unknown never counts). |
| `changed_lines_coverage_under: P` | Coverage of the lines the diff touched in a file is below P% (needs a diff). |
| `uncovered: true\|false` | A changed source file has known 0% coverage (`uncovered_includes_unknown: true` counts files in no report). |
| `hotspot: true\|false` | A changed source file is a hotspot. |

**Per-file keys** (`churn_over`, `complexity_over`,
`max_method_complexity_over`, `coverage_under`,
`changed_lines_coverage_under`, `uncovered: true`, `hotspot: true`, and next
to them `untested: true` and `changed_lines_over`) must all hold on **the
same** changed file. Pairing a metric with `changed_lines_over` keeps a
one-line edit to a big file from firing. The reason names the file:
`complexity above 150 and more than 20 changed lines in the same file:
app/models/purchase.rb (complexity 1186, 57 lines changed)`.

### Starter rules

These are the calibrated rules. `ripple init` writes them along with example
areas, area rules and a `changed_files_over` rule for you to adapt. With no
`rules:` key, the same thresholds apply as built-in `default.*` rules.

```yaml
rules:
  - id: complex-file
    when: { complexity_over: 150, changed_lines_over: 20 }
    risk: high
    require: [human_review]
  - id: hot-and-churning
    when: { hotspot: true, churn_over: 10 }
    risk: high
    require: [human_review]
  - id: hotspot-change
    when: { hotspot: true, changed_lines_over: 10 }
    risk: high
    require: [human_review]
  - id: huge-blast-radius
    when: { impacted_files_over: 1500 }
    risk: high
    require: [run_full_suite]
  - id: wide-blast-radius
    when: { impacted_files_over: 200 }
    risk: medium
  - id: complex-method
    when: { max_method_complexity_over: 25, changed_lines_over: 10 }
    risk: medium
  - id: untested-change
    when: { untested: true, changed_lines_over: 50 }
    risk: medium
    require: [add_tests]
  # Fire on a large share of ordinary commits, so they add an action, not risk.
  - id: global-config
    when: { touches_global: true }
    risk: low
    require: [run_full_suite]
  - id: frontend-backend
    when: { crosses_boundary: true }
    risk: low
  - id: forgot-a-partner
    when: { missing_cochange: true, cochange_min_confidence: 0.7 }
    risk: low
    require: [check_cochange_partners]
```

The built-ins also include `touches_tier: critical` → critical,
`touches_tier: high` → high, and reaching a critical area within 1 hop →
medium (at any depth → low).

**How these were calibrated, and why you must tune them.** We replayed
about 300 recent commits each of Gumroad and Mastodon (offline, from stored
`impact` reports, with ripple's rule semantics and each repo's own areas). These rules put HIGH or above on ~21% of Gumroad's commits and ~5% of
Mastodon's. Flagged commits were followed within 30 days by a fix commit
touching the same files more often than the rest: 63% vs 22% (Gumroad) and
40% vs 7% (Mastodon). No single set put both repositories in the same band,
so tune for yours: replay your own recent commits and adjust
`complexity_over`, `impacted_files_over` and `churn_over` until the share
of HIGH+ commits is one your reviewers can act on.

```sh
git worktree add ../replay HEAD && cd ../replay && mkdir -p /tmp/replay
for c in $(git rev-list --no-merges -n 300 HEAD | tail -r); do   # Linux: tac
  git checkout -q --detach "$c"
  until ripple history status | jq -e --arg c "$c" '.head == $c and .state == "ready"' >/dev/null; do sleep 0.2; done
  ripple rank --limit 1 >/dev/null                                 # refresh the hotspot ranking
  ripple impact --base "$c^" --sync --format json > "/tmp/replay/$c.json"
done
jq -r '.policy.risk // "low"' /tmp/replay/*.json | sort | uniq -c
jq -r '.policy.reasons[]?.rule' /tmp/replay/*.json | sort | uniq -c | sort -rn   # which rules fire
```

## Freshness

The daemon watches the work tree and re-indexes changed files in the
background. Before it answers, it makes sure it has seen every file event
that happened before the request, then waits for pending edits to be
indexed for **at most 150 ms** (`daemon.max_wait_ms`). If indexing is not
done by then, it answers from the current index and **says so**: the answer
carries `"stale": true` and the pending paths, and the report gets an
`INCOMPLETE stale_index` line ("re-run with --sync to wait"). A changed file
that is still pending is treated as wholly changed.

- `--sync` waits without a time limit and is never stale.
- `--no-wait` never waits.
- `file` and `rank` additionally wait up to 5 s for git history, coverage
  and the ranking on a fresh daemon; `impact` never waits for them (it
  reports them on the `not ready:` line instead).
- Git history (churn, co-change) loads on a low-priority background thread
  and is cached in `<git dir>/ripple/history.bin` (one file per worktree).
  CI (`--no-daemon`) only reads that cache; run `ripple history build` to
  create it.
- The daemon's socket, lock and log live in `$RIPPLE_SOCKET_DIR`, else
  `$XDG_RUNTIME_DIR/ripple`, else `/tmp/ripple-$UID`. Autostart is off when
  `RIPPLE_NO_DAEMON=1`, `CI` or `GITHUB_ACTIONS` is set.

## Performance

Measured on an Apple-silicon laptop that was under other load at the time,
release build, daemon mode.

| | Mastodon | Gumroad | GitLab |
|---|---:|---:|---:|
| Files in git (`git ls-files`) | 9,730 | 10,190 | 65,492 |
| First call, including building the index | 240 ms | 740 ms | 2.6 s |
| Warm `impact`, whole process, p50 / p95 | 4.6 / 5.3 ms | 4.1 / 4.8 ms | 25.6 / 27.1 ms ¹ |
| Save a method-body edit → fresh answer, p50 | 33 ms | 41 ms | 136 ms |
| Save an edit that adds or changes a constant reference or class structure → fresh answer | ~35 ms | ~71 ms | ~430 ms ² |
| Daemon memory (footprint) | 86 MB | 138 MB | 713 MB |

¹ A 30-file diff; about 20 ms of it is rule evaluation.
² Bounded by the Rubydex resolver, which re-resolves constants for such edits.

## JSON output

`impact`, `dependents` and `check --format json` emit one report model.
`ripple schema` prints its JSON Schema (draft 2020-12); the same file is
published as [`docs/report.schema.json`](docs/report.schema.json) (a test
keeps the two identical). Optional fields are omitted, never `null`. The
report is deterministic apart from `timing`.

**`schema_version` policy.** The report carries `"schema_version": 2`. The
version goes up when a field is removed or renamed, or its type or meaning
changes. New optional fields can appear without a bump. The published
schema is strict (`additionalProperties: false`) and describes exactly the
release it ships with: validate against the schema from the same release,
and have consumers ignore fields they do not know.

`file --format json` and `rank --format json` are not covered by the
schema and are not versioned yet; `rank` JSON has `ready` (false with
`not_ready` when there is no ranking) and the `formula`.

## Limitations and evidence

We measured what ripple gets right on real history. `crates/ripple-eval`
reproduces the dependents numbers (`ripple-eval breaks`); the rank and rule
numbers come from replay scripts kept outside this repository.

**Dependents are not complete.** Oracle: commits that removed or renamed a
Ruby method or constant, where other files of the same commit had to change
(GitLab 344 cases, Gumroad 114, Mastodon 81).

| Answer | Found at 1 hop | Within 3 hops |
|---|---:|---:|
| ripple, method-level | 64–67% | 75–81% |
| ripple, whole file | 64–67% | 75–83% |
| `rg -w NAME` | 100% | — |

ripple misses templates and JS/TS that use a Ruby name (receiver type
unknown), dynamic calls, and some unresolved constants. A word search finds
everything that mentions the name, with much more noise. ripple's
method-level answer prints the search to run (`also run: rg -w …`); run it.

**No measured benefit for agents.** Two trials compared agents with and
without ripple:

- Six edit tasks on Gumroad (renames, a signature change, an endpoint
  removal, a JSON key, nil handling): the two conditions produced identical
  diffs on 5 of 6 tasks.
- Review of 12 real commits that were later followed by a fix (6 Mastodon,
  6 Gumroad), plus 3 controls: 5 hits with ripple, 5 without. In 7 of the
  12, the file the later fix touched was not in ripple's output at all;
  mostly code that follows the same pattern with no dependency edge to the
  change.

So we do not claim that ripple improves agent edits or reviews. An empty or
short report says nothing about sibling code following the same pattern.

**Co-change is context.** In the review trial, `usually changes with` never
named a file that a later fix touched.

<a id="rank-vs-later-fix-commits"></a>**Rank vs later fix commits.** Share
of the files touched by later fix commits that were in the top 5/10/20% of
the ranking, and how many of the top 50 they were (P@50):

| | Top 5% | Top 10% | Top 20% | P@50 |
|---|---:|---:|---:|---:|
| Gumroad, shipped weights | .370 | .522 | .691 | .70 |
| Gumroad, churn alone | .339 | .496 | .635 | .66 |
| Mastodon, shipped weights | .255 | .396 | .583 | .56 |
| Mastodon, churn alone | .260 | .383 | .532 | .64 |
| Random (either) | ~.05 | ~.10 | ~.20 | ~.08 |

Both are far better than random. The shipped score is roughly on par with
ranking by recent churn alone, not better.

**Starter rules need tuning.** HIGH+ on ~21% of Gumroad and ~5% of
Mastodon commits; the follow-up-fix rate of flagged commits was 63% vs 22%
and 40% vs 7% (see [Starter rules](#starter-rules)). Those are two
repositories; your thresholds will differ.

**Other limits.**

- Coverage has only been exercised against synthetic reports.
- On a history dominated by bot or sync commits (GitLab-style), churn and
  co-change say little; co-change turns itself off there, churn stays on.
- No test selection: `tests` lists tests that depend on the change. It is
  not a minimal or a sufficient set to run.
- Static analysis only: `send`, `method_missing`, string-built constants,
  dynamic imports, hash keys, JSON keys and runtime configuration are
  invisible. `global_files` and the `INCOMPLETE` lines mark the worst cases.
- Method-level narrowing applies to Ruby methods and controller actions;
  JS/TS changes are whole-file.
- A cold index (CI) cannot list the dependents of a deleted Ruby file; a
  running daemon keeps them.
- Git submodules are not indexed. Unix only (Unix sockets, `flock`).

## How ripple differs from stud-finder

stud-finder is a Ruby gem that scores every file of a repository in one
batch run (fan-in, fan-out, complexity, churn over a window, coverage,
temporal coupling) and prints a ranked table, Ruby and JS/TS separately.

| | stud-finder | ripple |
|---|---|---|
| Ranked list | Yes | `ripple rank`: one list across Ruby and JS/TS, with a reason per row |
| Weights | Fixed weights over fan-in, fan-out, complexity, churn, coupling (and coverage when given) | Churn-heavy (3/1/0.5/1), from the replay above |
| Churn | Commits in a time window | Commits in the last 90 days of history, then total |
| Fan-out, temporal coupling in the score | Yes | No (co-change is reported per change instead) |
| Diff impact, dependents, rules | No | Yes |
| Running | One batch run over the repository | Daemon; milliseconds per query, kept fresh while you edit |

On Gumroad, the two tools' top 30 Ruby files share 13 files. We have not
measured whether either list predicts fixes better than the other.

## Environment

| Variable | Effect |
|---|---|
| `RIPPLE_ROOT`, `RIPPLE_CONFIG` | Same as `--root`, `--config`. |
| `RIPPLE_NO_DAEMON=1` | No daemon autostart (also when `CI` or `GITHUB_ACTIONS` is set). |
| `RIPPLE_SOCKET_DIR` | Directory for the daemon's socket, lock and log. |
| `NO_COLOR` | Plain text output. |
| `RIPPLE_EDGE_FILE`, `RIPPLE_EXTRACTORS`, `RIPPLE_DEBUG` | Extra edges from a file, extractor selection, daemon debug log (for tests and debugging). |
