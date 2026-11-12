# ripple in CI

**ripple reports; your workflow decides.** ripple computes what a change
reaches, which rules fire, the resulting risk level and the actions those
rules require (`human_review`, `run_full_suite`, `add_tests`,
`check_cochange_partners`, or your own). It never merges, blocks, labels or
requests reviewers by itself. Your workflow reads the exit code or the step
outputs and decides: fail the job, add a label, request a reviewer, post a
comment, run the full suite. ripple is one tool in that chain.

What the report is good for, and what not, is in the README's
[limitations and evidence](../README.md#limitations-and-evidence): the
dependents are about two thirds complete, and the rules flag review effort,
not correctness.

## Two entry points

| | `ripple impact --base REF` | `ripple check --base REF` |
|---|---|---|
| Daemon | Uses a running daemon; never starts one under CI (`CI` or `GITHUB_ACTIONS` set, or `RIPPLE_NO_DAEMON=1`) | Uses a running daemon; never starts one |
| Without a daemon | Indexes in-process | Indexes in-process |
| Formats | `text` (default), `json`, `markdown`, `github` | `github` under Actions, else `text`; also `markdown`, `json` |
| Fail-on default | never | `ci.fail_on` from `ripple.yml` (none: never) |
| Extra files | — | `--markdown-out PATH`, `--json-out PATH` |

Both compare `merge-base(REF, HEAD)` with the work tree, including
uncommitted and untracked files. Check out with `fetch-depth: 0` so the
merge base and the history exist.

## `--fail-on`

`--fail-on` takes risk levels, action names, or both; comma-separated or
repeated.

| Value | Exit 1 when |
|---|---|
| `low`, `medium`, `high`, `critical` | the change's risk is at or above that level (the lowest level given counts) |
| an action, e.g. `human_review` | a fired rule requires it |
| `never` | never; clears everything given before it |

```sh
ripple impact --base origin/main --fail-on high
ripple impact --base origin/main --fail-on human_review,run_full_suite
ripple impact --base origin/main --fail-on critical --fail-on human_review
```

An action that no rule in effect can require prints
`warning: --fail-on X: no rule in effect requires X` (a likely typo).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Ran; nothing reached `--fail-on`. |
| 1 | Ran; the risk or a required action reached `--fail-on`. The report is complete. |
| 2 | Usage, configuration, git or internal error, or a named file not in the index. No report. |

Treat 2 as "ripple could not tell you", not as "risky".

## `--format github`

Under GitHub Actions:

- **Annotations** on stdout: one `::warning` per changed file that a rule
  names, on its first changed line, with the highest-risk reason (at most
  50); one `::notice` with the summary line; `::error` when `--fail-on`
  trips.
- **Job summary**: the markdown report appended to `$GITHUB_STEP_SUMMARY`.
- **Step outputs** in `$GITHUB_OUTPUT`: `risk` (`low` … `critical`),
  `require` (comma-separated actions), `impacted`, `impacted_file_level`,
  `tests`, `changed` (counts).

```yaml
- id: ripple
  run: ripple impact --base "origin/${{ github.base_ref }}" --format github --fail-on critical
- if: always() && steps.ripple.outputs.risk == 'high'
  run: echo "::notice::high-risk change; requires ${{ steps.ripple.outputs.require }}"
```

`--format markdown` prints the same report as a PR comment body. It starts
with `<!-- ripple-impact-report -->`, so a workflow can find and update its
own comment.

## Example workflows

- [`examples/github-actions.yml`](../examples/github-actions.yml): zero
  config. Starts the daemon once, runs `impact --format github --fail-on`,
  posts a sticky markdown comment and sets one `risk:<level>` label.
- [`examples/github-workflow.yml`](../examples/github-workflow.yml): `ripple
  check` with `--markdown-out` / `--json-out`, a sticky comment and the JSON
  report as an artifact.

Both install a pinned release with `install.sh` (`RIPPLE_VERSION=ripple-v0.1.0`).

## Signals in CI

Rules about churn, co-change and hotspots need git history and the hotspot
ranking. `impact` and `check` never wait for them: a signal that is not
ready is reported on the `not ready:` line, configured rules that need it
do not fire (with a `rule_signal_not_ready` warning) and built-in rules
skip quietly. To have them in CI:

```sh
ripple history build          # writes <git dir>/ripple/history.bin
ripple daemon start           # optional: one index for several commands
ripple rank --limit 1 > /dev/null   # computes the hotspot ranking (waits up to 5 s for signals)
ripple impact --base "$BASE" --format github --fail-on high
```

Without a daemon, `impact`/`check` read the history cache but never build
it. Cache `.git/ripple/history.bin` between runs to skip the build. Coverage
is read from reports already in the work tree (download your test job's
coverage artifact into `coverage/` first); ripple never runs tests.

## JSON

`--format json` (and `check --json-out`) emit the report model described by
`ripple schema impact` ([docs/report.schema.json](report.schema.json)). The
fields a workflow usually needs:

```sh
ripple impact --base origin/main --format json | jq '{risk: .policy.risk, require: .policy.require, impacted: .counts.impacted}'
```

`schema_version` (now 2) goes up when a field is removed or renamed or its
type or meaning changes; new optional fields can appear without a bump.
Optional fields are omitted, never `null`. Validate against the schema of
the same release and ignore fields you do not know.
