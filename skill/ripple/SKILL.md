---
name: ripple
description: Lists files that depend on a change in this Rails + React repo (callers, subclasses, views, frontend consumers of an endpoint), each changed file's complexity, churn and coverage, and which risk rules fire (built-in, or from ripple.yml). Use before changing shared code and on your diff before you call a change done.
---

# ripple

ripple answers from a static index of the repo in a few milliseconds (the
first call starts a daemon). Its lists are not complete: it found about two
thirds of the files that real renames and removals had to change.

## Which command
- Before editing a method or action: `ripple dependents FILE --scope NAME`
  (omit `--scope` for the whole file).
- After editing: `ripple impact` (working tree vs HEAD), or
  `ripple impact --base REF` for a branch or commit range.
- Sizing up one file: `ripple file PATH`. Riskiest files: `ripple rank`.
- Many paths: `git diff --name-only | ripple impact -`; one JSON line per
  file: `... | ripple file -`.
- `--format json` for structure (`ripple schema impact|file|rank`). Do not add
  `--all` by default: the cut lists are enough; use it only to see one list
  in full.
- Just saved a file? Add `--sync` so the answer includes the edit.
- Something missing or slow (`not ready:`, no daemon)? `ripple doctor`.

## Reading the report
- `REMOVED … still mentioned`: methods you deleted or renamed that files
  still use. Fix each one.
- `dependents`: one line per file with the line that calls or references the
  changed code. Open each call line and ask whether the call still fits the
  new behaviour (arguments, return value, nil, errors, side effects).
- `also mentions NAME (no dependency edge)`: same name on a receiver ripple
  cannot type (views, mailers, `record.NAME`). For a rename, removal or
  signature change, treat them as dependents.
- `inherits/includes, does not call it`: usually nothing to do.
- `tests that depend on it`: tests that reach the change. Not a list of tests
  to run, and not proof the change is tested.
- `further`: 2-3 hops away, summarised. Context, not a to-do list.
- `usually changes with`: files that changed together in git history.
  Context only; check whether one needs the same edit.
- Metrics line (`cx 433 (max 88) · churn 41/90d · cov 62% · hotspot #3`):
  complexity, recent commits, coverage (absent = unknown, not 0%), hotspot
  rank. Tells you where to read slowly, not what is wrong.
- `risk` line: built-in rules plus any in `ripple.yml` (`ripple config show`
  lists them); about review effort, not correctness. Built-in areas
  (payments, auth, migrations...) are guessed from paths and file names.
- `INCOMPLETE` and `not ready:` lines: the answer is missing something
  (dynamic dispatch, a global file, a stale index, history still loading).

## What it does not see
- Code that follows the same pattern as your change without referencing it.
  An empty or short report says nothing about such sibling code: still
  search for the pattern you changed (`rg` for the method, key or idiom).
- `send`/`public_send`, symbols and strings (`:name`, `receive(:name)`),
  hash and JSON keys, JS/TS property names, metaprogramming, runtime config.
- Run the search on the `blind spots: … also run: rg -w …` line.
