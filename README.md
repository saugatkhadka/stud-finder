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
| `impact [FILES] [--base REF \| --staged \| --files A,B]` | Impact report. `--format text\|json\|markdown`, `--depth N` (0 = unlimited), `--min-confidence exact\|inferred\|heuristic`, `--all` (list everything), `--limit N` (cap every list but `changed`; counts stay exact), `--sync` (wait until every earlier edit is indexed, however long that takes), `--no-wait` (answer from the current index at once; may be marked stale), `--file-level` (see [Method-level analysis](#method-level-analysis)), `--scope NAME` (repeatable; with exactly one file: only these methods / controller actions changed). |
| `check --base REF` | CI entry point. `--format github\|markdown\|json\|text` (github is the default under `GITHUB_ACTIONS`), `--fail-on low\|medium\|high\|critical\|never` (default `ci.fail_on`), `--markdown-out PATH`, `--json-out PATH`, `--file-level`. Never starts a daemon, but uses one that is already running. |
| `dependents FILE` | The `impact` report of FILE as if it had changed (same sections and budgets). `--scope NAME` (repeatable): as if only that method / action changed. `--depth N`, `--all`, `--limit N`, `--format text\|json`. |
| `dependencies FILE` | Direct dependencies; `--depth N` (0 = all). |
| `schema` | Print the JSON Schema of the report (`schema_version` 2). |
| `why FROM TO` | Shortest dependency path between two files, in either direction. `--scope NAME`: only paths into that method / action of TO (says so when only a file-level path exists). |
| `areas FILE` | How `ripple.yml` classifies a file: areas, test, global, language. |
| `file FILE` | One file's profile in at most ~40 lines: language, areas / tier / owners, test and global flags, size and complexity, the 5 most complex methods with line ranges (and their coverage), churn, coverage and uncovered methods, dependents (count and the most depended-on few), dependencies, direct tests, co-change partners, hotspot rank and the state of every signal. `--format text\|json`. On a freshly started daemon it waits up to 5 s for git history, coverage and the ranking (`--no-wait`: answer at once); what is still not ready is named on a `not ready:` line. |
| `rank` | Riskiest files first (see [Hotspot ranking](#hotspot-ranking)). `--limit N` (20), `--path GLOB` (repeatable), `--area NAME`, `--by score\|churn\|complexity\|coverage`, `--format text\|json`. Each row says why it ranks where it does. Waits up to 5 s for git history and coverage to load (`--no-wait`: answer at once); with no ranking yet it says `ranking not ready: …` and exits 4. |
| `stats` | Index size, scope ranges (`files_with_scopes`, `scopes`), per-phase timings, extractor counters, warnings, daemon info. |
| `init [--force]`, `config check` | Starter config; validation plus how many files each area matches. |
| `daemon status\|start\|stop`, `serve [--foreground]` | Manage the daemon. |
| `history build`, `history status` | Build or update the git-history sidecar now (one-shot mode only reads it); show its state, size, and whether co-change is on. See [Git history in the daemon](#git-history-in-the-daemon). |

Global flags: `--root DIR` (or `RIPPLE_ROOT`; default is the enclosing git
work tree), `--config PATH` (or `RIPPLE_CONFIG`; use this file instead of
`<root>/ripple.yml`; the file must exist, and a separate daemon, keyed by
root and config path, serves it) and `--no-daemon` (index in this process).

Paths can be given relative to the current directory or as absolute paths.
They are normalised to paths relative to the repository root.

**Exit codes:**

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | `check` found a risk at or above the fail-on level |
| 2 | Usage error, configuration error, bad git ref, file not in the index, or internal error |
| 3 | `daemon status` found no daemon running |
| 4 | `rank` has no ranking yet: git history or coverage was still loading after the wait (or at once with `--no-wait`); it prints `ranking not ready: …` (JSON: `"ready": false`, `"not_ready"`) instead of an empty list |

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
| `daemon.max_wait_ms` | `150` | How long a daemon query waits for pending edits to be indexed before it answers from the current index, marked stale (see [Daemon and freshness](#daemon-and-freshness)). `--sync` waits without a limit. |
| `daemon.idle_timeout_minutes` | `30` | The daemon exits after this many minutes without a request; `0` = never. The next command starts a new one. |
| `history` | see right | Co-change from git history ("usually changes with this"), a separate signal that is never a graph edge and never feeds risk: `enabled` (`true`), `max_commits` (`20000`; `0` = all), `max_age_days` (none), `max_files_per_commit` (`30`; bigger commits are sweeps), `min_together` (`3`), `min_confidence` (`0.3`), `min_lift` (`2.0`), `limit` (`5`). Defaults chosen by `ripple-eval` replay. |
| `coverage` | see right | Existing test-coverage reports, read and never produced (ripple never runs tests): `enabled` (`true`), `paths` (`[]` = auto-detect `coverage/.resultset.json`, `coverage/lcov.info`, `coverage/lcov/*.lcov`, `coverage/coverage-final.json`; entries are files, directories or `*` globs in the last component), `max_age_days` (none; older reports are labelled stale, not hidden). |
| `rank.hotspot_percent` | `1` | Files in this top share of `ripple rank` are hotspots (the `hotspot` rule condition, `metrics.hotspot.hotspot`). |
| `rank.weights` | `churn: 3`, `complexity: 1`, `dependents: 0.5`, `uncovered: 1` | Weights of the factors of the `ripple rank` score (see [Hotspot ranking](#hotspot-ranking)); each 0 or more, `complexity` or `dependents` above 0. |
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
| `changed_lines_over: N` | There are more than N added plus deleted lines. Never fires without a diff (for example with `impact FILES`). Next to a per-file condition (below) or `untested: true`, it counts the lines of that same file: `{complexity_over: 300, changed_lines_over: 20}` skips a one-line edit to a huge file. |
| `crosses_boundary: true\|false` | An impacted file is on the other side of the Ruby/JS boundary from the changed file it traces back to. Ruby files and templates are the server side; JS/TS files are the client side. |
| `touches_global: true\|false` | A changed file matches `global_files`. |
| `untested: true\|false` | A changed, non-deleted source file has no test among its transitive dependents. With `true` next to a per-file condition or `changed_lines_over`, it must be that same file (`{untested: true, changed_lines_over: 50}`: a source file with no tests got more than 50 changed lines). |
| `min_confidence: C` | For this rule, ignores edges below C in `reaches*`, `impacted_files_over`, `crosses_boundary` and `untested`. |
| `churn_over: N` | A changed source file (not a test, not deleted) changed in more than N commits in the last 90 days of the history window. |
| `complexity_over: N` | A changed source file's complexity (sum of its methods' plus decisions outside methods) is above N. |
| `max_method_complexity_over: N` | A changed source file has a method of complexity above N. |
| `coverage_under: P` | A changed source file's line coverage is below P% (0–100). A file in no coverage report is unknown and never counts. |
| `changed_lines_coverage_under: P` | The coverable lines the diff added or changed in a file are covered below P%. Needs a diff (a `no_line_counts` warning otherwise). |
| `uncovered: true\|false` | A changed source file has known 0% coverage. With `uncovered_includes_unknown: true`, a file in no report counts too. |
| `hotspot: true\|false` | A changed source file is in the top `rank.hotspot_percent` of `ripple rank`. |
| `missing_cochange: true\|false` | A file outside the diff changed together with a changed file in at least `cochange_min_confidence` (0–1, default `0.5`) of that file's commits (from git history; graph neighbours count too). |

**Per-file conditions.** `churn_over`, `complexity_over`,
`max_method_complexity_over`, `coverage_under`, `changed_lines_coverage_under`,
`uncovered: true` and `hotspot: true` (and, next to any of them,
`untested: true` and `changed_lines_over`) must all hold on **the same**
changed file: a churny file plus a different complex file does not satisfy
`{churn_over: 20, complexity_over: 300}`. The reason names the files that
satisfy all of them, for example
`complexity above 300 and more than 20 changed lines in the same file: \`app/models/purchase.rb\` (complexity 433, 57 lines changed)`.
`uncovered: false` and `hotspot: false` hold when no changed file is
uncovered / a hotspot.

The last eight read **signals** besides the graph: complexity (from the
index), churn and co-change (git history), coverage (existing reports) and
the hotspot rank. A rule that needs a signal that is not ready (history
still building, no coverage report, signal disabled) does not fire and adds
one `rule_signal_not_ready` warning naming the signal and why. Each fired
rule's reason names the files and numbers, for example
`changed in more than 20 commits in the last 90 days: \`app/models/purchase.rb\` (41 commits)`.

**Built-in default rules**, used only when `rules` is absent or empty:

| Rule | Condition | Risk | Requires |
|---|---|---|---|
| `default.touches_critical` | Touches an area of tier critical | critical | `human_review` |
| `default.touches_high` | Touches an area of tier high or above | high | `human_review` |
| `default.complex_file` | One changed file: complexity over 150 and over 20 changed lines | high | `human_review` |
| `default.hotspot_churning` | One changed file: hotspot and churn over 10 | high | `human_review` |
| `default.hotspot_change` | One changed file: hotspot and over 10 changed lines | high | `human_review` |
| `default.impacted_over_1500` | More than 1500 impacted files | high | `run_full_suite` |
| `default.impacted_over_200` | More than 200 impacted files | medium | — |
| `default.reaches_critical_direct` | Reaches a critical area within 1 hop | medium | — |
| `default.complex_method` | One changed file: a method over 25 and over 10 changed lines | medium | — |
| `default.untested_change` | One changed file: no test reaches it and over 50 changed lines | medium | `add_tests` |
| `default.reaches_critical` | Reaches a critical area at any depth | low | — |
| `default.touches_global` | Touches a global file | low | `run_full_suite` |
| `default.missing_cochange` | A usual co-change partner (confidence 0.7+) is missing | low | `check_cochange_partners` |
| `default.crosses_boundary` | Crosses the Ruby/JS boundary | low | — |

The built-in metric rules skip quietly (no warning) until history and
metrics are ready. The thresholds come from replaying about 300 recent
commits each of two Rails apps (Gumroad, Mastodon) with these thresholds
(and medium-risk area rules): they put 21% of Gumroad's and 5% of Mastodon's commits
at high or above, and those commits were followed by a fix to the same
files about three times as often as the rest (63% vs 22%, 40% vs 7%).
Global files and boundary crossings fire on a large share of ordinary
commits (5–70%), so on their own they only add their required action.
No single generic set put both apps in a 10–15% band; tune
`complexity_over` and `impacted_files_over` to your codebase.

If no rule fires, the risk is low.

### Method-level analysis

Extractors report, per file, the line ranges of its methods and controller
actions (`Facts.scopes`), so a diff maps onto the methods it changed. The
graph itself stays file-level. Two mechanisms narrow a change confined to
named methods; both are conservative, so method-level results are always a
subset of file-level ones:

- **Controller actions.** Route, view, API-call and view-mount edges carry
  `from_scope` (the action a reference sits in) and `to_scope` (the action an
  edge targets). An edit inside `PostsController#show` reaches the views,
  routes and JS callers of `show`, not those of `index`.
- **Ruby mention filter (hop 1 only).** Ruby files also report which method
  names each of their methods mentions (calls with or without a receiver,
  symbols, `delegate`, templates included). For a seed changed only in
  methods `M`, `M` first grows inside the file and its superclasses/mixins:
  any method that mentions a name in `M` joins it. A direct dependent reached
  through a Ruby constant, require, inferred-association or view edge is kept
  only if it mentions a name in `M`. Matching is by name (Ruby has no static
  types), so `record.total` keeps every file that calls any `total`.
  Everything beyond the first hop is file-level, and dependents through
  inheritance (subclasses, includers) are always kept and followed in full.
  The seed is treated as wholly changed when a method in `M` is mentioned in
  the class body (callbacks, validations, `delegate`), is invoked implicitly
  (hooks such as `perform`/`call`/`to_s`/Pundit predicates, setters,
  operators), is mentioned nowhere in the workspace (called by convention),
  or when the file dispatches on `self` with a computed name (`send(x)`,
  `method_missing`). A dependent that dispatches with a computed name on
  another receiver (`record.public_send(x)`, any dynamic call in a template)
  always passes. `ruby.method_level: false` turns the filter (and the
  per-method scopes) off.
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
- `--file-level` turns all of this off (seeds, edge scopes and the mention
  filter). The report
  always carries `counts.impacted_file_level`, and the markdown headline reads
  e.g. "**212** impacted files (method-level; 1,362 if whole files are assumed
  changed)". The footer states which mode was used.

## Output (`schema_version: 2`)

`impact`, `dependents` and `check --format json` all emit one report model.
`ripple schema` prints its JSON Schema (draft 2020-12). The report is
deterministic, apart from `timing`. Every list is cut to a small budget
(see below) and its exact size is in `counts`. `--all` lists everything, and
`--limit N` caps every list at N, except `changed`, which keeps at least
its default budget (a larger N raises it).

### JSON shape

Abridged, from `dependents app/helpers/currency_helper.rb --scope string_to_price_cents` on Gumroad:

```jsonc
{
  "schema_version": 2,
  "summary": "1 changed file; 4 direct dependents (+55 only inherit/include); 1 more mention the name with no edge; 1197 more at hops 2-3; 1457 tests reach it; method-level (1300 if whole files changed); HIGH risk",
  "counts": { "changed": 1, "dependents": 59, "inherit_only": 55, "also_mentioned": 1, "relies_on": 0,
              "further": 1197, "further_notable": 182, "impacted": 1256, "impacted_file_level": 1300,
              "tests": 1457, "tests_direct": 1, "removed_methods": 0 },
  "truncated": true,                          // some list is shorter than its count (use --all)
  "changed": [{ "path": "app/helpers/currency_helper.rb", "status": "modified",
                "scopes": ["string_to_price_cents"],             // absent = the whole file
                "change": { "kind": "body", "methods": ["string_to_price_cents"] },
                "language": "ruby", "areas": [], "dependents": 62, "in_graph": true, "is_test": false, "is_global": false }],
  "removed_methods": [{ "file": "…", "method": "formatted_disputed_amount", "mentions_total": 11, "other_definitions": 0,
                        "mentions": [{ "file": "app/mailers/admin_mailer.rb", "line": 16, "how": "by_name" }] }],
  "dependents": [{ "file": "app/models/purchase.rb", "line": 7, "kind": "ruby_constant", "detail": "CurrencyHelper",
                   "confidence": "exact", "of": "app/helpers/currency_helper.rb", "inherit": true,
                   "uses_methods": ["string_to_price_cents"], "mention_line": 3415,   // the call, not the constant
                   "calls_on_constant": [96, 103],   // `Const.name(` call lines, when any (e.g. Link.fetch)
                   "from_scope": "…", "to_scope": "…", "crosses_boundary": true,   // when known / true
                   "language": "ruby", "areas": ["payments"] }],
  "inherit_only": ["app/models/charge.rb"],   // heirs / includers that never mention the changed methods
  "also_mentioned": [{ "name": "string_to_price_cents", "of": "…", "definitions": 1, "total": 1,
                       "singleton": true,     // only when the changed method is `def self.x`
                       "files": [{ "file": "app/modules/base_price/shared.rb", "line": 14, "how": "by_name" }] }],
  "relies_on": [{ "file": "…", "of": "…", "line": 3, "kind": "ruby_constant", "detail": "…", "confidence": "exact" }],
  "further": { "by_depth": { "2": 626, "3": 571 }, "by_language": { "ruby": 1041, "js": 142, "template": 14 },
               "by_area": { "payments": 28 }, "by_tier": { "critical": 30 },
               "files": [{ "file": "…", "depth": 2, "via": "…", "of": "…", "kind": "ruby_constant", "confidence": "exact",
                           "language": "ruby", "areas": ["payments"], "notable": ["high_tier"] }] },
  "tests": [{ "file": "spec/helpers/currency_helper_spec.rb", "depth": 1, "of": "…", "kind": "ruby_constant",
              "line": 5, "detail": "CurrencyHelper", "mention_line": 41 }],
  "incomplete": [{ "reason": "dynamic_dispatch", "file": "app/models/link.rb", "message": "…" }],
  "blind_spots": ["methods matched by name; receivers untyped", "JS/TS and send/try not checked", "also run: rg -w string_to_price_cents"],
  "policy": { "risk": "high", "reasons": [{ "rule": "…", "risk": "high", "message": "…", "require": [], "builtin": true, "evidence": { … } }],
              "require": [], "owners": ["@payments"], "areas": [{ "area": "payments", "tier": "critical", "files": 38, "min_depth": 1, … }] },
  "warnings": [{ "code": "global_file_changed", "message": "…", "file": "config/initializers/money.rb" }],
  "query": { "max_depth": 3, "min_confidence": "heuristic", "file_level": false, "all": false },
  "index": { "generation": 1, "nodes": 10150, "edges": 17783, "pending_changes": 0 },
  "timing": { "refresh_us": 0, "query_us": 203, "evaluate_us": 1236 }
}
```

What each section means:

- **`changed[].change.kind`**: `body` (only method bodies changed),
  `method_added`, `signature` (a `def` line changed), `method_removed`,
  `class_body` (code outside every method), or `whole_file` with a reason
  (`added`, `deleted`, `renamed`, `no_diff`, `unsupported_language`).
- **`removed_methods`**: methods that were deleted or renamed and are still
  mentioned. The lookup covers the same file, the direct dependents, and every
  file in the workspace that mentions the name. Files found only by name are
  listed when the name is rare: fewer than 3 other files define it.
  `other_definitions` says how common the name is. This appears in daemon and
  one-shot mode alike.
- **`dependents`**: hop 1 only, with one evidence line each. Order: files that
  call the changed method on the constant itself (`Link.fetch`), then files
  that mention the method, then files across the Ruby/JS boundary, then the
  rest. A `config/routes.rb` dependent shows the route line of the changed
  action. When there is no single action, `detail` reads `routes (N actions)`.
- **`inherit_only`**: subclasses and includers that never mention the changed
  methods. They stay in the impact (Ruby could dispatch to them), but are
  kept apart from callers.
- **`also_mentioned`**: files that mention the changed method's name but have
  no dependency edge, because the receiver's type is unknown
  (`purchase.charge.formatted_disputed_amount`, templates, mailers). This
  only applies to method-scoped changes. Files already listed are excluded.
  The files are listed only when the name is rare (`definitions` ≤ 2,
  counting the changed file) and the method is not a singleton. Otherwise only
  the count is given.
- **`relies_on`**: what the changed code itself uses (hop 1 forward). For a
  method-scoped change, only the edges inside those methods.
- **`further`**: hops 2–3 as counts by depth, language, area and tier, plus
  up to 10 notable files (high tier, crossing the boundary, inferred).
- **`tests`**: tests at hop 1 are listed, with the line that mentions the
  method. Tests further away are only counted (`counts.tests`).
- **`incomplete`**: why the answer may be missing files. Reasons:
  `global_file`, `not_in_graph`, `dynamic_dispatch`,
  `unresolved_references`, `stale_index`.
- **`blind_spots`**: present for method-scoped answers. It ends with the
  `rg -w` command that covers what static analysis cannot.
- **`policy`**: present only when `ripple.yml` defines rules or areas and a
  rule fired, or always for `check`. Rule semantics and CI exit codes are as
  described under Configuration.
- **`changed[].metrics`** (source files: not tests, not deleted, not "other"; each part absent when
  unknown or its signal is not ready):
  `{complexity, max_method, lines, churn: {commits, recent_commits, last_changed}, coverage: {percent, covered, coverable, changed_lines_percent, stale}, hotspot: {rank, of, percentile, hotspot}}`.
  `coverage` is absent for a file in no report (unknown, not 0%); `hotspot`
  is present only for a hotspot (`ripple file` shows any file's rank);
  `changed_lines_percent` needs a diff; `stale` (only when true) means the
  report predates HEAD or `coverage.max_age_days`.
- **`signals`**: `{complexity, churn, cochange, coverage, hotspot}`, each
  `{state: ready|building|loading|disabled|unavailable, reason?}`.
- **`usually_changes_with`**: at most 5 files outside the diff and its direct
  dependents that git history shows changing with a changed file:
  `{path, together, of, why}` (`why` names the changed file). Absent when there are none or co-change
  is not ready. (The daemon's envelope `history` keeps the raw data.)

Changing `ripple.yml` itself does not count as a change to analyse. It gives
one `ripple_config_changed` warning.

### Budgets

| List | Default size |
|---|---|
| `changed` | 30 |
| `dependents` | 30 |
| `inherit_only` | 10 |
| `also_mentioned[].files`, `removed_methods[].mentions` | 15 |
| `removed_methods` | 10 |
| `relies_on` | 10 |
| `further.files` | 10 |
| `tests` | 10 |
| `warnings` / `incomplete` per code | 5 |
| `policy.reasons[].evidence.impacted` | 1 |

The text format ends each cut list with `+N more (--all)`.

### Text

```
1 changed file; 0 direct dependents (+2 only inherit/include); 11 more mention the name with no edge; 801 more at hops 2-3; 629 tests reach it; method-level (821 if whole files changed); HIGH risk
changed M app/models/concerns/charge/disputable.rb  ruby  body formatted_disputed_amount  3 file dependents
inherits/includes, does not call it 2: app/models/charge.rb app/models/purchase.rb
also mentions `formatted_disputed_amount` (no dependency edge; receiver type unknown) 11 files, defined only here:
  app/mailers/admin_mailer.rb:16
  app/views/admin_mailer/chargeback_notify.html.erb:1
  …
further 801 files  hops 2:197 3:604  ruby:774 js:18 template:9  areas auth:4 payments:34 payouts:6  notable 63
  app/models/user.rb  hop 2 via app/models/purchase.rb  high_tier  [auth]
  +791 more (--all)
tests (static reach) 629: 1 direct
  spec/models/concerns/charge/disputable_spec.rb:216
  +628 more (--all)
risk HIGH  rules impacted_over_200, reaches_critical_direct, crosses_boundary  owners @payments @security
blind spots: methods matched by name; receivers untyped; JS/TS and send/try not checked; also run: rg -w formatted_disputed_amount
```

Each changed source file is followed by one metrics line, for example
`cx 433 (max 88) · churn 41/90d · cov 62% (changed lines 20%) · hotspot #3` (the rank
only for a hotspot).

The order is: summary, removed methods, incomplete, changed (with metrics),
dependents, inherit-only, also-mentions, relies-on, further, tests,
"usually changes with" (at most 5 lines), the one-line policy, warnings,
blind spots, and one `not ready:` line naming signals that are building,
loading or unavailable. The markdown format (for `check`) carries the same
sections.

Warning codes include:

- `global_file_changed`
- `changed_file_not_indexed`
- `deleted_file_not_indexed`
- `changed_file_ignored`
- `no_line_counts`
- `scope_unknown` (`--scope` names a method missing from the file's scope ranges)
- `deleted_file_still_imported` (cold index only: JS/TS files whose imports of
  a deleted file no longer resolve)
- `ripple_config_changed`
- `rule_signal_not_ready` (a rule needs churn, complexity, coverage, co-change or the hotspot rank and it is not ready)
- `config_invalid`
- extractor warnings about the changed files themselves

## Daemon and freshness

- **One daemon per repository.** It is started on demand by any command
  except `check`. Autostart is off when `RIPPLE_NO_DAEMON=1`, `CI` or
  `GITHUB_ACTIONS` is set; `--no-daemon` indexes in-process.
  - The socket, lock file and log live in `$RIPPLE_SOCKET_DIR`, else
    `$XDG_RUNTIME_DIR/ripple`, else `/tmp/ripple-$UID`. The directory has mode
    0700 and must be owned by you. The files are named
    `<fnv1a64(canonical root [+ NUL + --config path])>.sock|.lock|.log`. Socket paths are limited to
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
- **Freshness contract.** Every answer reflects every file edit that
  completed before the request was sent, or says which ones it does not.
  Before the daemon answers a query, it:
  1. writes a cookie file in `.git/` and waits for the watcher to report it,
     so every earlier file event has been delivered (skipped with
     `--no-wait`);
  2. re-stats the files named in the request, using mtime against the time
     each file was last examined;
  3. moves the pending batch to the front of the debounce queue and waits for
     it to be indexed, for at most `daemon.max_wait_ms` (default 150 ms). If
     the pass that would include the edits is predicted, from the last
     pass's duration, to end after that, it does not wait at all.

  If the index has not caught up, the daemon answers from the current
  snapshot and marks the response `"stale": true`, with `"pending"` (up to 50
  paths, sorted) and `"pending_count"`. In a stale `impact` answer, a changed
  file that is itself pending is treated as wholly changed: its diff hunks
  are not mapped onto method ranges indexed from older contents. `--sync`
  (`"sync": true`) waits without a time limit and is never stale.
  `--no-wait` (`"no_wait": true`) never waits. `stats` never waits.

  Edits are coalesced: each pass indexes every path delivered so far, so
  many editors cost one pass per indexer cycle, not one per edit. A query
  that names a file always sees that file's current contents once its answer
  is not stale; this is tested with a 200-iteration edit-then-query loop
  that has no sleeps.
- **Watcher latency on macOS.** The kernel hands file events to `fseventsd`
  at once only when about 16 or more are queued, otherwise after a 10 ms
  timer, so a lone cookie took about 11 ms to come back. The daemon follows
  each cookie with 24 `fchmod`s of it, which brings a round trip to about
  0.4 ms (p50). Kicked this way, an edit's event occasionally arrives just
  after a later cookie's, so the daemon does two round trips per sync. `ripple daemon stats` reports cookie round-trip percentiles
  and how many answers were fresh, waited or stale.
- **Diffs are computed in the daemon.** For `impact --base`, `--staged` and
  the default work-tree diff, the client sends the diff spec (`"diff"`) and
  the daemon answers it from a cache. It always writes a cookie first (even
  with `--no-wait`), so every file event before the request is seen. Then it re-runs
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
- **Connections.** Each connection has its own thread (at most 128 at
  once; more clients wait in the listen queue). A connection that sends
  nothing for 10 minutes is closed.
- **Deletions and renames.** While the daemon is running, a deleted file
  keeps the edges from its former dependents (their `detail` is prefixed
  `deleted:`). Its impact therefore still lists the files whose references
  are now broken. A cold index (CI, `--no-daemon`) cannot know these edges.
  It reports the deleted file with a `deleted_file_not_indexed` warning
  instead of silently reporting no impact. A rename is a deletion plus an
  addition. Both paths are seeds, and `touches` matches either one.
- **Case-insensitive file systems** (macOS): a wrongly-cased name never
  becomes a second workspace file.

### Git history in the daemon

Churn ("how often, and how recently, did this file change") and co-change
("usually changes with") come from `git log`, which takes from a fraction of
a second to over ten seconds on a large repository. The daemon therefore
loads them on a background thread. It runs at low priority (macOS QoS
utility, `nice 10` on Linux, and `nice 10` for the `git` it starts) and
never blocks a query. Until it is done, impact
replies say `"state": "building"` and carry no history data.

- **States.** `disabled` (`history.enabled: false`: no thread, no file),
  `building`, `ready`, `unavailable` (not a git repository, no commits,
  shallow clone, or git failed; the reason is given). Churn and co-change
  are flagged separately (`churn_available`, `cochange` +
  `cochange_reason`).
- **Sidecar.** `<git common dir>/ripple/history.bin` for the main work
  tree and `history-<name>.bin` next to it for a linked worktree (`<name>`
  as in `.git/worktrees/<name>`), written atomically (temporary file +
  rename). Each work tree writes only its own file, so worktrees at
  diverged HEADs do not overwrite each other's cache. A daemon starts from
  its own file at once, or, if that is missing or unusable, from any
  sibling's file whose head is an ancestor of its HEAD. If the file is
  behind HEAD, it is updated incrementally (only the new commits are read);
  with no usable file, it is rebuilt. An unreadable or corrupt file is
  rebuilt. Files of removed worktrees are deleted on the next save.
- **Updates.** HEAD moves are taken from the existing watcher (`HEAD`,
  `refs/heads/*` and `packed-refs`) and from the diff cache's HEAD check.
  They are debounced (500 ms), and deferred while a rebase, cherry-pick or
  revert is in progress (up to 60 s). The answer in use stays until the new
  one is ready (`updating: true` meanwhile). A newer HEAD move cancels an
  update that is still running.
- **Auto-off.** Co-change is switched off when the history is too short
  (fewer than 300 commits below the sweep cap) or mostly sweeps (more than
  25% of commits touch over `max_files_per_commit` files). In the `ripple-eval`
  replay, a sweep-heavy history (GitLab, 41% sweeps) added about twice as
  many wrong partners per right one as Mastodon (3% sweeps). Churn stays on.
- **One-shot mode** (`--no-daemon`, CI) only reads the sidecar, even when it
  is behind HEAD (`behind_head: true`), and never runs `git log`. Run
  `ripple history build` to create or update it.

Impact replies carry the result next to `result` (an additive field; old
clients ignore it):

```
"history": {"state": "ready", "head": "…", "commits": 20000, "churn_available": true, "cochange": true,
  "recent_days": 90,
  "churn": {"app/models/user.rb": {"commits": 412, "recent_commits": 9, "last_changed": 1794400000}},
  "usually_changes_with": [{"path": "spec/models/user_spec.rb", "via": "app/models/user.rb",
    "together": 120, "of": 412, "score": 0.29, "lift": 6.1, "last_together": 1794400000, "seeds": 1}],
  "lookup_us": 3}
```

`churn` lists every changed file. `usually_changes_with` leaves out the diff
itself and the files directly connected to it in the graph, which the report
already shows. `stats` has a `history` object with the state, build timings,
sidecar size, memory and counters.

### Coverage in the daemon

Coverage reports are read, never produced. The daemon loads them on a
low-priority background thread at start, then reloads only when
`sources_changed` says a report file appeared, disappeared or changed. That
check is a few `stat` calls, made on a watcher event under a report path or
at most every 5 seconds on a query; a query never parses a report. The new
coverage is swapped in as an `Arc`. States: `disabled` (`coverage.enabled:
false`), `loading`, `ready`, `unavailable` (no report found: picked up as soon as
one appears; a report that failed to parse is retried after 60 s). One-shot mode loads existing reports
inline (Gumroad's 900 KB SimpleCov file: about 20 ms) and never runs tests.
`stats` has a `coverage` object (state, reports, files, unmapped, load time,
memory).

### Hotspot ranking

`ripple rank` scores non-test Ruby and JS/TS files of the index. The formula
lives in one place, `rank_formula` in `crates/ripple-cli/src/signals.rs`, and
`rank --format json` prints it with the weights in effect:

```
score = 100 * (3*churn + 1*complexity + 0.5*dependents + 1*uncovered) / (sum of the weights used)
churn      = percentile of (commits in the last 90 days, then all commits in the window)
complexity = mean(percentile of file complexity, percentile of the most complex method);
             a file without methods counts 0 for both (the max-method guard)
dependents = percentile of direct non-test dependents (fan-in)
uncovered  = 1 - line coverage, only for files in a coverage report (else the weight is dropped;
             churn's too when git history is unavailable)
percentile(x) = share of ranked files with a strictly smaller value
hotspot    = rank <= ceil(files * rank.hotspot_percent / 100)
```

Percentiles rather than raw products keep one huge value from dominating.
The default weights (`rank.weights` in `ripple.yml`) come from a replay of 301
commits each of Mastodon and Gumroad, scored by which files later fix commits
touched: churn predicted best and dependents worst. With 3/1/0.5 the score
finds as many fix-touched files as churn alone in the top 5% (Gumroad 37% vs
34%, Mastodon 26% vs 26%) and more in the top 20% (69% vs 64%, 58% vs 53%);
the previous 2/2/1 trailed churn alone at the top.
The ranking is computed lazily, once per (index generation, history head,
coverage load), and cached. `impact` never computes it: it uses the cached
ranking and starts one background recompute when the cache is out of date
(at most every 2 s), so `metrics.hotspot` may be missing for a moment after
an edit. `ripple rank` waits for an up-to-date ranking; `ripple file` uses
the cached one and computes it only when there is none. The hotspot percentile
appears in `ripple file`, in `changed[].metrics.hotspot` and in the `hotspot`
rule condition.

### Protocol

The protocol is newline-delimited JSON over the Unix socket. Each request
gets one response line, in order, per connection:

```
→ {"id": 1, "method": "impact", "params": {"files": ["app/models/user.rb"], "format": "json", "limit": 50}}
← {"id": 1, "ok": true, "generation": 7, "server_us": 41, "stale": false, "synced": true, "pending_count": 0, "result": { …report… }}
← {"id": 2, "ok": false, "generation": 7, "server_us": 12, "error": {"code": "not_in_graph", "message": "…"}}
```

Methods:

| Method | Params |
|---|---|
| `impact` | `files`, `changed` (entries may carry `hunks`: `[{start, count, removes_definition?}]`), `diff` (`{"base": "REF"}`, `{"staged": true}` or `{}` for the work tree; not combined with `files`/`changed`), `scopes`, `file_level`, `max_depth`, `min_confidence`, `format`, `color`, `limit`, `max_listed`, `sync`, `no_wait`, `max_wait_ms` |
| `dependents`, `dependencies` | `file`, `scopes` (dependents only), `depth`, `min_confidence`, `sync`, `no_wait`, `max_wait_ms` |
| `why` | `from`, `to`, `scopes`, `sync`, `no_wait`, `max_wait_ms` |
| `areas` | `file`, `sync`, `no_wait`, `max_wait_ms` |
| `file` | `file`, `sync`, `no_wait`, `max_wait_ms` (the result is JSON; the client renders text) |
| `rank` | `limit`, `path` (globs), `area`, `by` (`score`\|`churn`\|`complexity`\|`coverage`), `sync`, `no_wait`, `max_wait_ms` |
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
  scope ranges count as whole-file. The Ruby mention filter matches by name
  and applies to direct dependents only; a dependent that only `require`s the
  file and reaches its methods by a convention (Thor subcommands, for
  example) is dropped.
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
| `RIPPLE_CONFIG` | Config file, the same as `--config`. |
| `RIPPLE_NO_DAEMON=1` | Disables daemon autostart. `CI` and `GITHUB_ACTIONS` do the same. |
| `RIPPLE_SOCKET_DIR` | Directory for the socket, lock and log files. |
| `NO_COLOR` | Plain text output. |
| `RIPPLE_EDGE_FILE` | Extra edges from a file: `FROM TO [KIND [CONF [LINE [DETAIL]]]]` per line; either endpoint may be `path#method` (`#@body` = class body); `@inherit FROM TO ...` adds an inheritance edge; `@scope FILE NAME START END` declares a method's line range. `#` after whitespace starts a comment. For tests, benchmarks and custom integrations. |
| `RIPPLE_EXTRACTORS` | Comma list of the extractors and linkers to run, by name. For debugging and benchmarks. |
| `RIPPLE_DEBUG` | Logs watcher events and indexing batches to the daemon log. |
