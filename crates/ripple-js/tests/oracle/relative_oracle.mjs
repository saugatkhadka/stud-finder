#!/usr/bin/env node
// Independent oracle for ripple-js relative-import resolution.
//
// Dependency-free: finds relative import statements with regexes, resolves a
// seeded random sample with plain `fs` checks (no code shared with the Rust
// resolver), and compares with the edges ripple-js produced.
//
//   node relative_oracle.mjs <repo> <edges.tsv> <dump-output.txt> [--seed N] [--sample N]
//
// <edges.tsv>  from `dump <repo> --edges edges.tsv`
// <dump-output.txt>  stdout of `dump <repo> --unresolved` (used to confirm that
//                    imports the oracle cannot resolve were reported as warnings)
//
// Exit status is non-zero if any disagreement is found.

import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import path from "node:path";

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  if (i < 0) return dflt;
  const v = args[i + 1];
  args.splice(i, 2);
  return Number(v);
};
const seed = opt("--seed", 1);
const sampleSize = opt("--sample", 300);
const [repo, edgesPath, dumpPath] = args;
if (!repo || !edgesPath || !dumpPath) {
  console.error("usage: relative_oracle.mjs <repo> <edges.tsv> <dump-output.txt> [--seed N] [--sample N]");
  process.exit(2);
}

// ---- the file set: same rule as ripple's default `ignore` globs ---------------
const IGNORED_ROOTS = new Set(["node_modules", "vendor", "tmp", "log", "public", "coverage", ".git", "dist", "build"]);
const tracked = execFileSync("git", ["ls-files", "-co", "--exclude-standard", "-z"], { cwd: repo, maxBuffer: 1 << 28 })
  .toString()
  .split("\0")
  .filter(Boolean);
const files = new Set(tracked.filter((f) => !IGNORED_ROOTS.has(f.split("/")[0]) && isFile(path.join(repo, f))));
function isFile(p) {
  try { return statSync(p).isFile(); } catch { return false; }
}
function isDir(p) {
  try { return statSync(p).isDirectory(); } catch { return false; }
}

// ---- find relative import statements by regex --------------------------------
const CODE = /\.(?:[cm]?[jt]sx?|vue)$/;
const patterns = [
  /(?:^|[\s;}])(?:import|export)\b[^'"`\n;]*?\bfrom\s*['"](\.{1,2}(?:\/[^'"\n]*)?)['"]/gm,
  /(?:^|[\s;])import\s*['"](\.{1,2}(?:\/[^'"\n]*)?)['"]/gm,
  /\brequire\(\s*['"](\.{1,2}(?:\/[^'"\n]*)?)['"]\s*\)/g,
  /\bimport\(\s*['"](\.{1,2}(?:\/[^'"\n]*)?)['"]\s*\)/g,
];
const statements = [];
for (const f of [...files].filter((f) => CODE.test(f)).sort()) {
  const text = readFileSync(path.join(repo, f), "utf8");
  const seen = new Set();
  for (const re of patterns) {
    re.lastIndex = 0;
    for (const m of text.matchAll(re)) {
      const line = text.slice(0, m.index + m[0].indexOf(m[1])).split("\n").length;
      const key = `${line}\0${m[1]}`;
      if (seen.has(key)) continue;
      seen.add(key);
      statements.push({ file: f, line, spec: m[1] });
    }
  }
}
console.log(`found ${statements.length} relative import statements in ${new Set(statements.map((s) => s.file)).size} files`);

// ---- seeded sample (mulberry32 + Fisher-Yates) --------------------------------
function mulberry32(a) {
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rnd = mulberry32(seed);
const pool = statements.slice();
for (let i = pool.length - 1; i > 0; i--) {
  const j = Math.floor(rnd() * (i + 1));
  [pool[i], pool[j]] = [pool[j], pool[i]];
}
const sample = pool.slice(0, sampleSize);

// ---- plain-fs resolution (Node + TS rules) -------------------------------------
const EXT = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".vue", ".d.ts"];
const JS_TO_TS = { ".js": [".ts", ".tsx"], ".jsx": [".tsx"], ".mjs": [".mts"], ".cjs": [".cts"] };
function resolveOnDisk(fromFile, rawSpec) {
  const spec = rawSpec.split(/[?#]/)[0];
  let base = path.posix.normalize(path.posix.join(path.posix.dirname(fromFile), spec)).replace(/\/+$/, "");
  if (base === "") base = ".";
  const asDir = spec.endsWith("/") || spec === "." || spec === "..";
  const tryFile = (p) => (isFile(path.join(repo, p)) ? p : null);
  if (!asDir) {
    const exact = tryFile(base);
    if (exact) return exact;
    for (const e of EXT) { const r = tryFile(base + e); if (r) return r; }
    const ext = path.posix.extname(base);
    for (const alt of JS_TO_TS[ext] ?? []) { const r = tryFile(base.slice(0, -ext.length) + alt); if (r) return r; }
  }
  if (isDir(path.join(repo, base))) {
    try {
      const pkg = JSON.parse(readFileSync(path.join(repo, base, "package.json"), "utf8"));
      for (const field of ["module", "main", "types"]) {
        if (typeof pkg[field] === "string") {
          const m = path.posix.normalize(path.posix.join(base, pkg[field]));
          const r = tryFile(m) ?? EXT.map((e) => tryFile(m + e)).find(Boolean) ?? EXT.map((e) => tryFile(m + "/index" + e)).find(Boolean);
          if (r) return r;
        }
      }
    } catch { /* no package.json */ }
    for (const e of EXT) { const r = tryFile(`${base}/index${e}`); if (r) return r; }
  }
  return null;
}

// ---- ripple's output ----------------------------------------------------------
const edgesBy = new Map(); // from -> [{to, detail}]
for (const line of readFileSync(edgesPath, "utf8").split("\n").filter(Boolean)) {
  const [from, to, kind, , , detail] = line.split("\t");
  if (!edgesBy.has(from)) edgesBy.set(from, []);
  edgesBy.get(from).push({ to, kind, detail });
}
const unresolved = new Set(); // "file:line:spec"
for (const line of readFileSync(dumpPath, "utf8").split("\n")) {
  const m = line.match(/^\s*(\S+):(\d+): cannot resolve relative import '(.*)'$/);
  if (m) unresolved.add(`${m[1]}:${m[2]}:${m[3]}`);
}

// ---- compare ------------------------------------------------------------------
let agree = 0;
const disagreements = [];
const outcomes = { resolved: 0, unresolved: 0 };
for (const s of sample) {
  const want = resolveOnDisk(s.file, s.spec);
  const have = edgesBy.get(s.file) ?? [];
  const label = `${s.file}:${s.line} '${s.spec}'`;
  if (want === null) {
    outcomes.unresolved++;
    const reported = unresolved.has(`${s.file}:${s.line}:${s.spec}`);
    const phantom = have.find((e) => e.detail.replace(/ \(type\)$/, "") === s.spec);
    if (reported && !phantom) agree++;
    else disagreements.push(`${label}: oracle=unresolved ripple=${phantom ? "edge to " + phantom.to : "no warning"}`);
  } else {
    outcomes.resolved++;
    const onFileSet = files.has(want);
    const edge = have.find((e) => e.to === want);
    if (want === s.file) {
      // self import: no edge by design
      if (!edge) agree++; else disagreements.push(`${label}: self-import but ripple has an edge`);
    } else if (!onFileSet) {
      disagreements.push(`${label}: oracle resolves to ${want} which is outside the file set`);
    } else if (edge) agree++;
    else disagreements.push(`${label}: oracle=${want} ripple=${have.filter((e) => e.detail.startsWith(s.spec)).map((e) => e.to).join(",") || "no edge"}`);
  }
}
console.log(`sample=${sample.length} seed=${seed} oracle-resolved=${outcomes.resolved} oracle-unresolved=${outcomes.unresolved}`);
console.log(`agree=${agree} disagree=${disagreements.length}`);
for (const d of disagreements) console.log("  DISAGREE " + d);
process.exit(disagreements.length ? 1 : 0);
