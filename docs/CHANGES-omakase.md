# Omakase: user-visible changes

ripple now works well with no configuration. It stays scriptable and can
install its own agent skill.

## Zero configuration

- **Built-in areas.** Each one applies only where its paths exist.
  `ripple config show` lists them with their file counts.

  | Area | Tier | What it matches |
  |---|---|---|
  | `payments` | medium | keyword-matched files |
  | `auth` | high | keyword-matched files, plus `app/models/user.rb` and the Devise, Doorkeeper, OmniAuth and Rack::Attack initializers |
  | `migrations` | medium | `db/migrate`, `db/post_migrate`, schema files, with `ee/` variants |
  | `api` | medium | `controllers/api`, `graphql`, `lib/api` |
  | `config` | low | |
  | `jobs` | low | |
  | `mailers` | low | |
  | `shared_components` | low | |

- **Built-in rules.**
  - There are 17 `default.*` rules. Each relative threshold also has an absolute floor.
  - Every report now has a risk section, including when there is no `ripple.yml`.
  - Built-in rules never add `rule_signal_not_ready` or `no_line_counts` warnings. `dependents` no longer warns about `no_line_counts`.
- **Merge semantics.** A `ripple.yml` extends the built-ins:

  | In `ripple.yml` | Effect |
  |---|---|
  | An area or rule with the same name or id as a built-in | Replaces the built-in |
  | Any other area or rule | Is added |
  | `defaults: {areas: false}` | Drops all built-in areas |
  | `defaults: {rules: false}` | Drops all built-in rules |
  | `defaults.disable: [names]` | Drops individual built-ins. Rule ids work with or without `default.`. An unknown name is an error. |
  | `defaults.codeowners: false` | Stops CODEOWNERS from being read |

  A built-in rule about a built-in area is dropped along with that area.
- **CODEOWNERS.**
  - Read from `.github/`, the repository root, `docs/` or `.gitlab/`. GitLab `[Section]` lines are skipped.
  - The last matching pattern wins.
  - Owners of changed files are added to the report's `owners`.
- **Coverage auto-detection.** When `coverage.paths` is empty, ripple looks for reports in:
  - `coverage/`
  - `tmp/coverage/`
  - `{spec,test,frontend,client,web}/coverage/`
  - `app/{javascript,frontend}/coverage/`
- **Test patterns.** The default `tests` globs cover the rspec, minitest, jest and vitest layouts, including `__tests__`, `*.test.*` and `*.spec.*`.
- **`ripple config show [--format json]`** prints the configuration in effect:
  - the source file
  - built-in and configured areas, with file counts
  - rules and where each came from
  - CODEOWNERS
  - coverage reports found
  - this repository's top 1, 2, 5 and 10% cut-offs for complexity, maximum method complexity and dependents
- **`ripple init [--force] [--stdout]`** writes the repository's effective defaults as comments, followed by the full reference, also commented. The written file changes nothing until you edit it.
- **`ripple config check`** marks built-in areas and rules. It lists only the built-in areas that match files, and warns only about configured areas that match nothing.

## Relative thresholds

- **New `when` keys:**

  | Key | What it measures |
  |---|---|
  | `impacted_files_over_percent: P` | impacted files as a share of the repository's non-test files |
  | `complexity_top_percent: P` | per file |
  | `max_method_complexity_top_percent: P` | per file |
  | `dependents_top_percent: P` | per file |
  | `dependents_over: N` | per file |

  The three `*_top_percent` keys rank a file within this repository's distribution. Their messages give the cut-off and the rank, for example "#12 of 3069".
- **Calibration.**
  - **Method.** An offline replay of the built-in rules over the stored `impact --base C^` reports for about 300 recent commits each of Mastodon and Gumroad, run with `calfix/sim_zero.py`:
    - built-in areas were matched against paths with the same glob semantics as ripple
    - the cut-offs came from each repository's `config show`
    - per-file conditions were checked on the same file
    - a grid search over the thresholds kept the setting that minimised the larger distance from 12.5% across the two repositories
  - **Result.**

    | | HIGH or above | Commits with a fix within 30 days: flagged | : not flagged |
    |---|---|---|---|
    | Mastodon | 8.3% | 20% | 8% |
    | Gumroad | 16.9% | 65% | 24% |
    | Before calibration, Mastodon | 10.6% | | |
    | Before calibration, Gumroad | 39.2% | | |

  - **No single rule set put both repositories in the 10–15% range.** Gumroad's commits are larger, and its graph is denser:
    - 90th-percentile impacted files: 1,253 for Gumroad against 352 for Mastodon
    - Gumroad's base rate of follow-up fixes is about 3 times higher
  - **Changes made by the calibration:**
    - `payments` became medium. Keyword matching hit 30% of Gumroad's commits.
    - `hotspot_churning` now needs churn above 20.
    - `hotspot_change` now needs more than 50 changed lines.
    - The huge-blast-radius rule moved from 25% to 40%. Its id is now `default.impacted_over_40pct`.

## CI contract

- **`impact --base REF --fail-on LEVEL|ACTION,…`** fails on a risk level, on required actions such as `human_review`, or on both. `never` clears what was given before it. An action that no rule in effect requires produces a warning.
- **`check --fail-on`** accepts the same values.
- **Exit codes:**

  | Code | Meaning |
  |---|---|
  | 0 | ok |
  | 1 | fail-on was reached. Also: `doctor` found something to fix, or `skill status` found an outdated or edited skill. |
  | 2 | error |
  | 3 | `daemon status` found no daemon |
  | 4 | `rank` is not ready |

- **`impact --format markdown|github`.** `github` writes:
  - annotations
  - the job summary
  - the `risk`, `require`, `impacted`, `tests` and `changed` outputs
  - `::error` when fail-on trips
- **`examples/github-actions.yml`.** A zero-config workflow:
  - starts the daemon once
  - runs `impact --format github --fail-on`
  - posts a sticky markdown comment
  - sets a `risk:<level>` label

## Unix behaviour

- Data goes to stdout; diagnostics and hints go to stderr. Colour appears only on a terminal and never with `NO_COLOR` or `TERM=dumb`.
- `--format json` now also works for `history status`, `config show`, `doctor` and `skill status`.
- **`ripple schema impact|file|rank`.** Copies are in `docs/`. Tests check that the copies are identical and that real output validates against them.
- **Paths on stdin:**
  - `impact -` reads changed paths.
  - `ripple file -` reads paths and writes one JSON object per line. Exit 2 if any path failed.
- **Other additions:**
  - `-q/--quiet`
  - help text with examples and the exit codes
  - `ripple completions SHELL`

## Agent skill

- **`ripple skill install|update|uninstall|status|print`.** Options:
  - `--project` (the default) or `--user` (`$HOME`)
  - `--agent claude|codex|agents`, repeatable
- **Version stamp and manifest.** The skill is stamped with the ripple version and written with a `.ripple-skill` manifest that holds a hash.
  - An edited skill is never overwritten or removed without `--force`.
  - Uninstall removes only the directories it leaves empty.
- **SKILL.md** now mentions:
  - built-in rules
  - stdin paths
  - `doctor`
  - the schema kinds

## First run and `doctor`

- **First-run hint.** On the first text-format `impact` in a repository without `ripple.yml`, a single line on stderr points to `config show` and `skill install`. It appears only when stderr is a terminal, never with `--quiet`, and once per repository.
- **`ripple doctor` (alias `status`)** reports on:
  - the configuration
  - the daemon
  - index freshness
  - each signal
  - the skill locations

  It lists what to fix (exit 1) and optional suggestions. It never starts a daemon.

## Performance

- On GitLab, `impact --base HEAD~1` rule evaluation went from about 30 ms to about 3.4 ms (commit aba1c22). With the built-in rules and CODEOWNERS it is about 4 ms.
