# Reading the report, and the metrics

## The report, section by section

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
risk MEDIUM  rules impacted_over_5pct, …
blind spots: methods matched by name; receivers untyped; JS/TS and send/try not checked; also run: rg -w string_to_price_cents
not ready: coverage unavailable (no coverage report found in coverage/ …)
```

In order (JSON field names in parentheses):

- **Summary line.** Counts of each section, whether the answer is
  method-level, and the risk. "Impacted" means dependents at any depth,
  excluding tests and the changed files.
- **`REMOVED … still mentioned`** (`removed_methods`). Methods the diff
  deleted or renamed that some file still mentions by name. The most likely
  breakages.
- **`INCOMPLETE`** (`incomplete`). Reasons the lists may be missing files:
  `global_file` (a file like `Gemfile.lock` or an initializer that anything
  may depend on), `not_in_graph`, `dynamic_dispatch` (the file uses `send`,
  `method_missing` or `eval`), `unresolved_references`, `stale_index`.
- **`changed`.** One line per changed file: status, language, line counts,
  what changed (`body m` = only method `m`'s body, `added m`, `signature`,
  `class body`, or `whole file` with the reason) and its file-level
  dependent count, then one metrics line for source files. Three or more
  plain non-source files of one directory (locale JSON, fixtures) are one
  line: `changed 21 files in app/javascript/mastodon/locales/ (other)`. The
  JSON lists every file.
- **`dependents`.** Files one hop away, each with the line of the reference,
  the edge kind and its confidence. For a method-level change, files that
  call the changed method come first, with the line of the call. **Read each
  call line and ask whether the call still fits the new behaviour.**
- **`inherits/includes, does not call it`** (`inherit_only`). Subclasses and
  includers that never mention the changed methods. Ruby could still
  dispatch to them, so they stay in the impact.
- **`also mentions NAME`** (`also_mentioned`). Files that mention the
  changed method's name with no dependency edge, because ripple cannot tell
  the receiver's type (`purchase.charge.total`, views, mailers). Listed only
  when the name is rare; otherwise counted. For a rename, removal or
  signature change, treat these like dependents.
- **`relies on`** (`relies_on`). What the changed code itself uses (one hop
  forward).
- **`further`.** Files 2–3 hops away, as counts by depth, language and area,
  plus up to 10 notable ones (high-tier area, crossing the Ruby/JS boundary,
  inferred edge). Context, not a to-do list.
- **`tests that depend on it`** (`tests`). Tests that reach the change
  through the graph. Hop-1 tests are listed with the line that mentions the
  method; the rest are counted. Not coverage and not a list of tests to run.
- **`usually changes with (not in the diff)`** (`usually_changes_with`). Up
  to 5 files outside the diff and its direct dependents that git history
  shows changing with a changed file. Context only.
- **`risk`** (`policy`). The highest risk among fired rules, the rule ids,
  required actions and suggested owners (areas' owners and CODEOWNERS). With
  `--format json` each rule has a message naming the files and numbers.
- **`warning:`** lines. For example `global_file_changed`,
  `changed_file_not_indexed`, `no_line_counts`, `rule_signal_not_ready`.
- **`blind spots`.** For method-level answers: what ripple cannot see, and
  the `rg -w` search that covers it. Run it.
- **`not ready:`** Signals still building or unavailable (below).

Each list is cut to a small budget (30 dependents, 10 tests, 10 further
files, …) and ends with `+N more (--all)`. The counts are never cut.

## Metrics

Computed for changed source files (Ruby, JS/TS, templates; not tests, not
deleted files, not "other" files such as JSON or YAML). The text form is one
line, `cx 433 (max 88) · churn 41/90d · cov 62% (changed lines 20%) ·
hotspot #3`; a part is left out when it is unknown or its signal is not
ready. `ripple file PATH` shows all of them for one file.

### Lines

- **What:** code lines: lines with something other than whitespace or a
  comment (templates: non-blank lines).
- **Why:** size. Big files take longer to review and hide more.
- **Read it:** as context for complexity. A 3,000-line model with
  complexity 1,200 is not unusual; a 100-line file with complexity 80 is
  dense.

### Complexity (`cx`) and max method complexity (`max`)

- **What:** cyclomatic-style. Each method scores `1 + decisions` (`if`,
  `unless`, `case`/`when`, loops, `&&`/`||`, `&.`, `rescue`, iterator
  blocks; for JS/TS, ESLint's "classic" rules). Ruby aims to match RuboCop's
  `Metrics/CyclomaticComplexity`. `cx` is the file's sum of method scores
  plus decisions outside methods; `max` is its most complex method.
- **Why:** more paths through the code means more cases a change can break
  and a test can miss.
- **Read it:** `cx` grows with file size; `max` tells you whether there is
  one knot. `ripple file` lists the 5 most complex methods with line ranges.

### Churn (`churn 41/90d`)

- **What:** commits that touched the file: in the 90 days **before the
  newest commit in the history window** (not before today), and in the whole
  window (the newest 20,000 non-merge commits by default,
  `history.max_commits`).
- **Why:** code that changed often recently tends to change again, often to
  fix something. In our replay, ranking by recent churn alone did about as
  well as any weighted combination we tried.
- **Read it:** high recent churn means the area is in motion: check for
  concurrent work and recent fixes. Churn includes sweeps (mass renames,
  formatting); look at `git log` before drawing conclusions.

### Coverage (`cov 62% (changed lines 20%)`)

- **What:** line coverage from existing reports (see
  [CONFIG.md](CONFIG.md#what-is-detected-automatically)). `changed lines` is
  coverage of the lines this diff added or changed (needs a diff). `stale`
  means the report is older than HEAD's commit or than
  `coverage.max_age_days`; line numbers may have moved.
- **Unknown is not zero.** A file in no report has no coverage value. It
  does not count as 0% in rules or the rank unless a rule sets
  `uncovered_includes_unknown: true`.
- **Why:** low coverage on changed lines means the change may ship untested.
- **Read it:** compare `changed lines` with the file's percentage. Coverage
  has only been exercised against synthetic reports so far; check a few files
  by hand.

### Hotspot rank (`hotspot #3`)

`ripple rank` scores every non-test Ruby and JS/TS file. Each factor is a
percentile (the share of ranked files with a strictly smaller value), so one
huge value cannot dominate:

```
score      = 100 * (3*churn + 1*complexity + 0.5*dependents + 1*uncovered) / (sum of the weights used)
churn      = percentile of (commits in the last 90 days, then total commits)
complexity = mean(percentile of file complexity, percentile of max method complexity)
dependents = percentile of direct non-test dependents
uncovered  = 1 - line coverage, only for files in a coverage report (else its weight is dropped;
             churn's too when git history is unavailable)
hotspot    = rank <= ceil(files * rank.hotspot_percent / 100)    (top 1% by default)
```

- `rank --format json` prints the formula with the weights in effect
  (`rank.weights`). The weights were chosen by replaying ~300 commits each
  of Gumroad and Mastodon; the result is roughly on par with ranking by
  churn alone.
- `hotspot #N` appears in the metrics line only for hotspots; `ripple file`
  shows any file's rank and percentile.
- **Read it:** a hotspot is a file to review slowly, not a file that is
  wrong.

### Dependents counts

- **What:** in `changed`, the file-level count of direct dependents
  (fan-in). In the summary: direct dependents of the changed methods,
  those that only inherit or include, mentions with no edge, files further
  away, and tests. `impacted_file_level` in the JSON is the impact if every
  changed file were wholly changed.
- **Why:** a rough blast radius.
- **Read it:** a count of the references ripple found, not a complete count.
  In a dense Rails app 2–3 hops reach a large part of the app; read
  `further` as a sketch.

### Co-change ("usually changes with")

- **What:** files that changed in the same commits as a changed file:
  `with X in 50 of its 63 commits (79%)`. Commits touching more than 30
  files are ignored as sweeps; a partner must also beat its own base rate
  (`history.min_lift`). Files already in the diff or directly connected in
  the graph are left out.
- **Why:** a usual partner missing from the diff can be a forgotten edit (a
  locale file, a serializer, a spec).
- **Read it:** as context. In our review trial it never named a file that a
  later fix touched. It turns itself off when history is short (under 300
  commits) or mostly sweeps (over 25% of commits).

## Signal states

Each signal (`complexity`, `churn`, `cochange`, `coverage`, `hotspot`) has a
state, shown in `signals` (JSON), on the `not ready:` line and by
`ripple doctor`:

| State | Meaning |
|---|---|
| `ready` | Loaded and current. |
| `building` / `loading` | Being computed in the background (git history on a fresh daemon, coverage being read, the ranking waiting for them). `impact` does not wait; `file` and `rank` wait up to 5 s. |
| `unavailable` | Cannot be computed; the reason says why (no coverage report, shallow clone, not a git repository, co-change off because of sweeps). |
| `disabled` | Turned off in `ripple.yml` (`history.enabled: false`, `coverage.enabled: false`). |

A configured rule that needs a signal that is not ready does not fire and
adds one `rule_signal_not_ready` warning; built-in rules skip quietly.
