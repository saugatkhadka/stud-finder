# Configuration reference

ripple needs no configuration. Without a `ripple.yml` it applies built-in
areas and rules, reads CODEOWNERS and finds coverage reports in the usual
places. A `ripple.yml` (or `.ripple.yml`) at the repository root extends or
overrides those defaults; `--config PATH` / `RIPPLE_CONFIG` uses another file
(served by its own daemon).

- `ripple config show [--format json]`: what is in effect here (merged
  built-ins and file), with area file counts, rule origins, CODEOWNERS,
  coverage reports found and this repository's percentile cut-offs.
- `ripple config check`: validate the file and show what each area matches.
- `ripple init [--force] [--stdout]`: write a `ripple.yml` that lists this
  repository's effective defaults and then every key, all commented out. It
  changes nothing until you edit it.

Unknown keys and invalid values are errors that name the key and the line.
[`examples/ripple.yml`](../examples/ripple.yml) uses every `when` key.

## What is detected automatically

| What | How |
|---|---|
| Files | `git ls-files` plus untracked, not ignored files (a plain directory walk outside git), minus `ignore`. |
| Ruby autoload roots | Rails defaults (`app/*`, `lib`), plus `ruby.autoload_roots`. |
| JS/TS aliases | `tsconfig.json` / `jsconfig.json` at the root and in workspaces, bundler aliases, workspace packages. |
| Routes | `config/routes.rb`, `config/routes/**/*.rb`. |
| Areas | The built-in areas below, each only where its paths match files. |
| Owners | CODEOWNERS in `.github/`, the root, `docs/` or `.gitlab/` (GitLab `[Section]` lines skipped; the last matching pattern wins). |
| Coverage | `.resultset.json` (SimpleCov), `lcov.info`, `lcov/*.lcov`, `coverage-final.json` (Istanbul) under `coverage/`, `tmp/coverage/`, `{spec,test,frontend,client,web}/coverage/`, `app/{javascript,frontend}/coverage/`. |
| History | `git log` of the work tree, in the background. Off for shallow clones. |
| Tests | RSpec, Minitest, Jest, Vitest, Cypress and Storybook layouts (the `tests` default). |

## Merging with the built-ins

| In `ripple.yml` | Effect |
|---|---|
| An area or rule with a built-in's name or id | Replaces the built-in |
| Any other area or rule | Is added |
| `defaults: {areas: false}` | Drops all built-in areas |
| `defaults: {rules: false}` | Drops all built-in rules (a `rules:` list alone does not) |
| `defaults: {disable: [name, …]}` | Drops single built-ins: area names, rule ids with or without `default.`. An unknown name is an error. |
| `defaults: {codeowners: false}` | Ignores CODEOWNERS |

A built-in rule about a built-in area (`default.schema_change` uses
`migrations`) goes away with that area.

```yaml
version: 1
defaults:
  disable: [payments, complex_method]   # an area and a rule
areas:
  payments:                             # or redefine it with your own paths
    tier: critical
    paths: [app/models/billing/**, app/services/payments/**]
    owners: ["@acme/payments"]
```

## Top-level keys

| Key | Default | Meaning |
|---|---|---|
| `version` | `1` | Must be 1. |
| `ignore` | `node_modules/**`, `vendor/**`, `tmp/**`, `log/**`, `public/**`, `coverage/**`, `.git/**`, `dist/**`, `build/**` | Never analysed, in addition to `.gitignore`. Setting it replaces the list. |
| `areas.<name>` | built-ins | `paths` (globs), `tier` (`low`/`medium`/`high`/`critical`, required), `owners`, `description`. |
| `global_files` | `config/initializers/**`, `config/application.rb`, `config/environment.rb`, `config/environments/**`, `config/boot.rb`, `Gemfile`, `Gemfile.lock`, `package.json`, `yarn.lock`, `package-lock.json`, `pnpm-lock.yaml` | Files anything may depend on in ways no static graph sees. A change to one is reported `INCOMPLETE global_file`. |
| `tests` | `spec/**`, `test/**`, `**/spec/**`, `**/test/**`, `**/__tests__/**`, `**/__mocks__/**`, `**/*.test.*`, `**/*.spec.*`, `**/*.cy.*`, `**/*_spec.rb`, `**/*_test.rb`, `**/*.stories.*`, `cypress/**` | Listed under "tests that depend on it", never counted as impacted. |
| `rules[]` | built-ins | `id`, `when` (`{}` always fires), `risk`, `require: [action, …]`, `description`. |
| `defaults` | all on | `areas`, `rules`, `codeowners` (booleans), `disable: [names]`. |
| `query.max_depth` | `3` | Hops to follow from a changed file; `0` = unlimited. |
| `query.min_confidence` | `heuristic` | Ignore edges below this (`exact` > `inferred` > `heuristic`). |
| `ci.fail_on` | none | Default `--fail-on` level of `ripple check` (never fails when unset). |
| `ci.github` | `true` | `check` writes `$GITHUB_STEP_SUMMARY` and outputs under Actions. |
| `ci.max_listed` | `50` | Files listed in the markdown report. |
| `daemon.max_wait_ms` | `150` | How long a query waits for pending edits before answering marked stale. |
| `daemon.idle_timeout_minutes` | `30` | The daemon exits after this long without a request; `0` = never. |
| `daemon.sleep_after_seconds` | `300` | After this long without requests or file edits the daemon saves its index and serves it memory-mapped (see [ARCHITECTURE.md](ARCHITECTURE.md#sleep)); `0` = never. |
| `history.enabled` | `true` | Git history signals (churn, co-change). |
| `history.max_commits` | `20000` | Newest non-merge commits read; `0` = all. |
| `history.max_age_days` | none | Also only commits younger than this. |
| `history.max_files_per_commit` | `30` | Bigger commits are sweeps: they count for churn, not co-change. |
| `history.min_together` | `3` | A co-change partner changed together at least this often… |
| `history.min_confidence` | `0.3` | …in at least this share of the file's commits… |
| `history.min_lift` | `2.0` | …and this many times its own base rate. |
| `history.limit` | `5` | Partners shown per report. |
| `coverage.enabled` | `true` | Read coverage reports. |
| `coverage.paths` | `[]` | Report files or directories; `[]` = auto-detect (above). A `*` is allowed in the last component. |
| `coverage.max_age_days` | none | Older reports are labelled stale (still used). |
| `rank.hotspot_percent` | `1` | Top share of `ripple rank` that counts as a hotspot. |
| `rank.weights` | `churn: 3, complexity: 1, dependents: 0.5, uncovered: 1` | Rank weights, each ≥ 0; `complexity` or `dependents` must be above 0. |
| `ruby.enabled` | `true` | Ruby extractor. |
| `ruby.autoload_roots` | `[]` | Extra autoload roots beyond `app/*` and `lib`. |
| `ruby.namespace_definition_limit` | `3` | A constant defined in more files than this is a shared namespace; references resolve only to its Zeitwerk-canonical file. |
| `ruby.method_level` | `true` | Narrow a change confined to some methods to the files that mention them (first hop). `false` = file-level only. |
| `js.enabled` | `true` | JS/TS extractor. |
| `js.tsconfig` | `[]` | tsconfig/jsconfig files to read aliases from; `[]` = auto-detect. |
| `js.aliases` | `{}` | Extra import aliases: prefix → directory. |
| `boundary.enabled` | `true` | Rails ↔ JS linker (routes, API calls, view mounts). |
| `boundary.routes` | `config/routes.rb`, `config/routes/**/*.rb` | Route files. |
| `boundary.http_callees` | `[]` | Extra callees whose URL argument is matched to routes (built-ins: `fetch`, `axios[.verb]`, `$.ajax`, `request`). Dotted and call-chain forms: `api`, `api().get`. |
| `boundary.url_prefixes` | `[]` | Only URLs starting with one of these are matched; `[]` = any URL starting with `/`. |
| `boundary.url_bases` | `/`, `/api/` | Bases tried for URLs written without a leading slash. |
| `boundary.link_edges` | `false` | Also link plain navigation links (`href={Routes.x_path()}`). |

The daemon watches `ripple.yml`. Changing `ruby`, `js`, `boundary` or
`ignore` makes it rebuild its index; other keys apply from the next query.

**Globs** follow gitignore rules: `*` stays in one directory, `**` crosses
directories, a pattern without `/` matches at any depth, a leading `/`
anchors to the root, a directory matches everything under it. In `touches`
and `reaches`, a name that is an area means that area; anything containing
`/ * ? [ { .` is a glob.

## Rules

A rule fires when every key in its `when` holds. The change's risk is the
highest risk among fired rules (`low` if none fired); the `require` lists of
fired rules are merged. Tier conditions mean "this tier or higher", so for
the built-in tier ladders only the highest rung that fired is reported.

### `when` keys

| Key | Holds when |
|---|---|
| `touches: [area\|glob]` | A changed file is in a listed area or matches a glob (a rename's old path too). |
| `touches_tier: T` | A changed file is in an area of tier T or higher. |
| `reaches: [area\|glob]` | An impacted file (not a test, not changed) is in a listed area or glob. |
| `reaches_tier: T` | The same, for an area of tier T or higher. OR-ed with `reaches`. |
| `within: N` | Limits `reaches` / `reaches_tier` to N hops (may exceed `query.max_depth`). |
| `min_confidence: C` | For this rule, ignore edges below C. |
| `impacted_files_over: N` | More than N impacted non-test files. |
| `impacted_files_over_percent: P` | Impacted non-test files are more than P% of the repository's non-test files. |
| `changed_files_over: N` | More than N changed files. |
| `changed_lines_over: N` | More than N added + deleted lines (needs a diff). Next to a per-file key: lines of that same file. |
| `crosses_boundary: true\|false` | An impacted file is on the other side of the Ruby/JS boundary. |
| `touches_global: true\|false` | A changed file matches `global_files`. |
| `untested: true\|false` | A changed source file has no test among its dependents. |
| `missing_cochange: true\|false` | A file that changed with a changed file in at least `cochange_min_confidence` (default `0.5`) of its commits is not in the diff. |
| `cochange_min_confidence: F` | Threshold for `missing_cochange`. |
| `churn_over: N` | A changed source file had more than N commits in the last 90 days of history. |
| `complexity_over: N` | A changed source file's complexity is above N. |
| `complexity_top_percent: P` | A changed source file's complexity is in the top P% of this repository. |
| `max_method_complexity_over: N` | A changed source file has a method above N. |
| `max_method_complexity_top_percent: P` | Its most complex method is in the top P% of this repository. |
| `dependents_over: N` | A changed source file has more than N direct non-test dependents. |
| `dependents_top_percent: P` | Its direct dependents are in the top P% of this repository. |
| `coverage_under: P` | A changed source file's line coverage is below P% (unknown never counts). |
| `changed_lines_coverage_under: P` | Coverage of the lines the diff touched in a file is below P% (needs a diff). |
| `uncovered: true\|false` | A changed source file has known 0% coverage. |
| `uncovered_includes_unknown: true` | With `uncovered`: files in no report count as uncovered. |
| `hotspot: true\|false` | A changed source file is a hotspot (top `rank.hotspot_percent` of `ripple rank`). |

**Per-file keys** (`churn_over`, `complexity_over`, `complexity_top_percent`,
`max_method_complexity_over`, `max_method_complexity_top_percent`,
`dependents_over`, `dependents_top_percent`, `coverage_under`,
`changed_lines_coverage_under`, `uncovered: true`, `hotspot: true`, and next
to them `untested: true` and `changed_lines_over`) must all hold on **the
same** changed file. Pairing a metric with `changed_lines_over` keeps a
one-line edit to a big file from firing. The reason names the file, for
example `complexity above 150 and more than 20 changed lines in the same
file: app/models/purchase.rb (complexity 1186, 57 lines changed)`.

The three `*_top_percent` keys rank a file among this repository's non-test
source files; their messages give the cut-off and the rank ("#12 of 3069").
`ripple config show` prints the top 1/2/5/10% cut-offs.

**Signals not ready.** A rule that needs history, coverage or the hotspot
ranking while that signal is still loading does not fire. A configured rule
adds one `rule_signal_not_ready` warning; the built-in rules skip quietly.
A configured rule with `changed_lines_over` and no diff (for example
`dependents`) adds a `no_line_counts` warning.

## Built-in areas

Each applies only where its paths match files. Disable one with
`defaults: {disable: [NAME]}`, or redefine it under `areas:` to replace it.

| Name | Tier | Paths |
|---|---|---|
| `payments` | medium | Paths under `{app,lib,ee/app,ee/lib,src}` containing one of: billing, payment, Payment, payout, Payout, stripe, Stripe, paypal, PayPal, braintree, invoice, Invoice, refund, Refund, checkout, Checkout, purchase, Purchase |
| `auth` | high | Paths under the same roots containing one of: session, Session, authenticat, Authenticat, password, Password, oauth, OAuth, omniauth, two_factor, TwoFactor, webauthn, WebAuthn, login, Login; `app/models/user.rb`; `config/initializers/{devise,doorkeeper,omniauth,rack_attack}*` |
| `migrations` | medium | `{db,ee/db}/{migrate,post_migrate}/**`, `db/schema.rb`, `db/structure.sql`, `db/*_schema.rb`, `db/*_structure.sql` |
| `api` | medium | `{app,ee/app}/controllers/api/**`, `{app,ee/app}/graphql/**`, `{lib,ee/lib}/api/**` |
| `config` | low | `config/initializers/**`, `config/environments/**`, `config/application.rb`, `config/routes.rb`, `config/routes/**`, `config/*.yml` |
| `jobs` | low | `{app,ee/app}/{jobs,workers,sidekiq}/**` |
| `mailers` | low | `{app,ee/app}/mailers/**`, `{app,ee/app}/views/**/*_mailer/**` |
| `shared_components` | low | `{app,ee/app}/{javascript,frontend,assets/javascripts}/**/components/**`, `{app,ee/app}/assets/javascripts/vue_shared/**`, `src/components/**` |

`payments` and `auth` are guesses from file names. `payments` is medium, not
high, because in a payments product the keywords match about a third of the
code (30% of Gumroad's commits). Redefine either with your own paths and
tier.

## Built-in rules

Disable one with `defaults: {disable: [ID]}` (with or without `default.`),
or define a rule with the same id to replace it.

| Id | When | Risk | Requires |
|---|---|---|---|
| `default.touches_critical` | `touches_tier: critical` | critical | `human_review` |
| `default.touches_high` | `touches_tier: high` (built-in: `auth`) | high | `human_review` |
| `default.schema_change` | `touches: [migrations]` | medium | `human_review` |
| `default.touches_medium` | `touches_tier: medium` | medium | |
| `default.complex_file` | `complexity_top_percent: 2`, `complexity_over: 50`, `changed_lines_over: 30` | high | `human_review` |
| `default.hotspot_churning` | `hotspot: true`, `churn_over: 20` | high | `human_review` |
| `default.hotspot_change` | `hotspot: true`, `changed_lines_over: 50` | high | `human_review` |
| `default.impacted_over_40pct` | `impacted_files_over_percent: 40`, `impacted_files_over: 100` | high | `run_full_suite` |
| `default.impacted_over_5pct` | `impacted_files_over_percent: 5`, `impacted_files_over: 25` | medium | |
| `default.widely_used` | `dependents_top_percent: 1`, `dependents_over: 10`, `changed_lines_over: 10` | medium | |
| `default.complex_method` | `max_method_complexity_top_percent: 2`, `max_method_complexity_over: 10`, `changed_lines_over: 10` | medium | |
| `default.reaches_critical_direct` | `reaches_tier: critical`, `within: 1` | medium | |
| `default.reaches_critical` | `reaches_tier: critical` | low | |
| `default.untested_change` | `untested: true`, `changed_lines_over: 50` | medium | `add_tests` |
| `default.touches_global` | `touches_global: true` | low | `run_full_suite` |
| `default.missing_cochange` | `missing_cochange: true`, `cochange_min_confidence: 0.7` | low | `check_cochange_partners` |
| `default.crosses_boundary` | `crosses_boundary: true` | low | |

No built-in area is critical, so `touches_critical` and the `reaches_critical`
rules fire only once you define a critical area.

### Calibration

The thresholds are relative to the repository, with absolute floors, so the
same rules fit a 2,000-file and a 60,000-file codebase. They were set by an
offline replay of the stored `impact --base C^` reports for about 300 recent
commits each of Mastodon and Gumroad: built-in areas matched with ripple's
glob semantics, cut-offs from each repository's `config show`, per-file
conditions checked on the same file, and a grid search that minimised the
larger distance from 12.5% of commits at HIGH or above across the two.

| | HIGH or above | Followed by a fix within 30 days: flagged | : not flagged |
|---|---:|---:|---:|
| Mastodon | 8.3% | 20% | 8% |
| Gumroad | 16.9% | 65% | 24% |

No single rule set put both repositories in a 10–15% band: Gumroad's
commits are larger and its graph is denser (90th-percentile impacted files
1,253 against 352), and its base rate of follow-up fixes is about three
times Mastodon's. Before calibration the same replay gave 10.6% and 39.2%;
calibration made `payments` medium, raised `hotspot_churning` to churn above
20 and `hotspot_change` to more than 50 lines, and moved the huge-blast-radius
rule from 25% to 40% (`default.impacted_over_40pct`).

These are two repositories. To tune for yours, replay your own recent
commits and adjust until the share of HIGH+ commits is one your reviewers
can act on:

```sh
git worktree add ../replay HEAD && cd ../replay && mkdir -p /tmp/replay
for c in $(git rev-list --no-merges -n 300 HEAD | tail -r); do   # Linux: tac
  git checkout -q --detach "$c"
  until ripple history status --format json | jq -e --arg c "$c" '.head == $c and .state == "ready"' >/dev/null; do sleep 0.2; done
  ripple rank --limit 1 >/dev/null                                 # refresh the hotspot ranking
  ripple impact --base "$c^" --sync --format json > "/tmp/replay/$c.json"
done
jq -r '.policy.risk // "low"' /tmp/replay/*.json | sort | uniq -c
jq -r '.policy.reasons[]?.rule' /tmp/replay/*.json | sort | uniq -c | sort -rn   # which rules fire
```

## Starter rules with absolute thresholds

To adapt when you write your own:

```yaml
rules:
  - id: complex-file
    when: { complexity_over: 150, changed_lines_over: 20 }
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
  - id: untested-change
    when: { untested: true, changed_lines_over: 50 }
    risk: medium
    require: [add_tests]
  - id: poorly-covered
    when: { touches_tier: high, coverage_under: 50 }
    risk: high
  # Fire on a large share of ordinary commits: add an action, not risk.
  - id: global-config
    when: { touches_global: true }
    risk: low
    require: [run_full_suite]
```
