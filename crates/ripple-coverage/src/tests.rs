use super::*;
use std::fs;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn cfg() -> CoverageConfig {
    CoverageConfig::default()
}

const KNOWN: &[&str] = &[
    "app/models/user.rb",
    "ee/app/models/user.rb",
    "lib/a.rb",
    "app/javascript/x.js",
    "packages/web/src/foo.js",
    "packages/api/src/foo.js",
    "packages/web/src/bar.js",
];

fn load(root: &Path, c: &CoverageConfig) -> Coverage {
    Coverage::load(root, c, KNOWN.iter().copied()).unwrap()
}

fn fc(covered: u32, coverable: u32) -> FileCoverage {
    FileCoverage { covered, coverable }
}

#[test]
fn simplecov_legacy_array_form() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let rs = format!(
        r#"{{"RSpec": {{"coverage": {{"{}/app/models/user.rb": [1, null, 0, 5, null]}}, "timestamp": 1700000000}}}}"#,
        root.display()
    );
    write(root, "coverage/.resultset.json", &rs);
    let c = load(root, &cfg());
    assert_eq!(c.file("app/models/user.rb"), Some(fc(2, 3)));
    assert_eq!(c.reports()[0].format, Format::SimpleCov);
    assert_eq!(c.reports()[0].measured_at, 1700000000);
    assert_eq!(c.line("app/models/user.rb", 1), Some(LineState::Covered));
    assert_eq!(
        c.line("app/models/user.rb", 2),
        Some(LineState::NotCoverable)
    );
    assert_eq!(c.line("app/models/user.rb", 3), Some(LineState::Uncovered));
    assert_eq!(
        c.line("app/models/user.rb", 99),
        Some(LineState::NotCoverable)
    );
}

#[test]
fn simplecov_lines_form_merges_suites_and_takes_newest_timestamp() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let r = root.display();
    let rs = format!(
        r#"{{
      "RSpec": {{"coverage": {{"{r}/app/models/user.rb": {{"lines": [0, null, 0, 0], "branches": {{"[:if, 0, 1, 2, 3, 4]": {{"[:then, 1, 2, 2, 2, 4]": 0}}}}}},
                              "{r}/lib/a.rb": {{"lines": [1, "ignored", 0]}}}},
                "timestamp": 1700000000}},
      "Minitest": {{"timestamp": 1700000500, "coverage": {{"{r}/app/models/user.rb": {{"lines": [null, null, 3, 0, 0]}}}}}}
    }}"#
    );
    write(root, "coverage/.resultset.json", &rs);
    let c = load(root, &cfg());
    // Line 1: coverable (0 + null), uncovered; 3: 0 + 3 > 0; 4: 0 + 0; 5: only in suite 2.
    assert_eq!(c.file("app/models/user.rb"), Some(fc(1, 4)));
    assert_eq!(c.line("app/models/user.rb", 3), Some(LineState::Covered));
    assert_eq!(
        c.line("app/models/user.rb", 2),
        Some(LineState::NotCoverable)
    );
    assert_eq!(
        c.file("lib/a.rb"),
        Some(fc(1, 2)),
        "\"ignored\" is not coverable"
    );
    assert_eq!(c.reports()[0].measured_at, 1700000500);
    assert_eq!(c.reports()[0].entries, 2);
    assert_eq!(c.file_count(), 2);
}

#[test]
fn lcov_info_and_lcov_dir() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/lcov.info",
        &format!(
            "TN:\nSF:{}/app/javascript/x.js\nFN:1,f\nFNDA:1,f\nDA:1,1\nDA:2,0\r\nDA:3,4,abcdef\nBRDA:3,0,0,1\nLF:3\nLH:2\nend_of_record\nSF:packages/web/src/foo.js\nDA:1,0\nend_of_record\n",
            root.display()
        ),
    );
    write(
        root,
        "coverage/lcov/project.lcov",
        "SF:lib/a.rb\nDA:1,1\nDA:2,1\nend_of_record\n",
    );
    let c = load(root, &cfg());
    assert_eq!(c.reports().len(), 2);
    assert!(
        c.reports()
            .iter()
            .all(|r| r.format == Format::Lcov && r.error.is_none())
    );
    assert_eq!(c.file("app/javascript/x.js"), Some(fc(2, 3)));
    assert_eq!(c.file("packages/web/src/foo.js"), Some(fc(0, 1)));
    assert_eq!(c.file("lib/a.rb"), Some(fc(2, 2)));
}

#[test]
fn istanbul_coverage_final() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let body = format!(
        r#"{{"{r}/app/javascript/x.js": {{"path": "{r}/app/javascript/x.js",
            "statementMap": {{"0": {{"start": {{"line": 1, "column": 0}}, "end": {{"line": 1, "column": 9}}}},
                             "1": {{"start": {{"line": 2, "column": 0}}, "end": {{"line": 4, "column": 1}}}},
                             "2": {{"start": {{"line": 2, "column": 4}}, "end": {{"line": 2, "column": 9}}}}}},
            "s": {{"0": 1, "1": 0, "2": 2}}, "fnMap": {{}}, "f": {{}}, "branchMap": {{}}, "b": {{}}}},
           "{r}/lib/a.rb": {{"statementMap": "garbage"}}}}"#,
        r = root.display()
    );
    write(root, "coverage/coverage-final.json", &body);
    let c = load(root, &cfg());
    assert_eq!(c.reports()[0].format, Format::Istanbul);
    assert_eq!(
        c.file("app/javascript/x.js"),
        Some(fc(2, 2)),
        "line 2 has a statement that ran"
    );
    assert_eq!(c.reports()[0].malformed, 1);
    assert_eq!(c.file("lib/a.rb"), None);
}

#[test]
fn istanbul_skipped_next_to_lcov_info_by_default() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:1,1\nend_of_record\n",
    );
    write(root, "coverage/coverage-final.json", "{}");
    let c = load(root, &cfg());
    assert_eq!(c.reports().len(), 1);
    // Named explicitly, it is read.
    let c = load(
        root,
        &CoverageConfig {
            paths: vec![
                "coverage/coverage-final.json".into(),
                "coverage/lcov.info".into(),
            ],
            ..cfg()
        },
    );
    assert_eq!(c.reports().len(), 2);
}

#[test]
fn unknown_is_not_zero() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:1,0\nDA:2,0\nend_of_record\nSF:app/javascript/x.js\nend_of_record\n",
    );
    let c = load(root, &cfg());
    let zero = c.file("lib/a.rb").unwrap();
    assert_eq!(zero, fc(0, 2));
    assert_eq!(zero.percent(), Some(0.0));
    assert_eq!(
        c.file("app/models/user.rb"),
        None,
        "not in any report: unknown"
    );
    assert_eq!(c.lines("app/models/user.rb", &[(1, 10)]), None);
    // In a report with no coverable lines: known, no percentage.
    assert_eq!(c.file("app/javascript/x.js"), Some(fc(0, 0)));
    assert_eq!(c.file("app/javascript/x.js").unwrap().percent(), None);
}

#[test]
fn line_range_queries() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    // 200 lines: odd lines coverable, multiples of 3 covered (among odd ones).
    let mut s = String::from("SF:lib/a.rb\n");
    for l in (1..=200).step_by(2) {
        s += &format!("DA:{l},{}\n", u32::from(l % 3 == 0));
    }
    s += "end_of_record\n";
    write(root, "coverage/lcov.info", &s);
    let c = load(root, &cfg());
    let brute = |ranges: &[(u32, u32)]| {
        let mut f = fc(0, 0);
        for l in 1..=300u32 {
            if ranges.iter().any(|&(a, b)| a <= l && l <= b) && l <= 200 && l % 2 == 1 {
                f.coverable += 1;
                f.covered += u32::from(l % 3 == 0);
            }
        }
        f
    };
    for ranges in [
        vec![(1, 1)],
        vec![(2, 2)],
        vec![(1, 64)],
        vec![(60, 70)],
        vec![(63, 130)],
        vec![(1, 300)],
        vec![(150, 400)],
        vec![(10, 20), (15, 30), (5, 6)],
        vec![(100, 90)],
        vec![(0, 3)],
        vec![(64, 64), (65, 65), (128, 129)],
    ] {
        assert_eq!(
            c.lines("lib/a.rb", &ranges),
            Some(brute(&ranges)),
            "{ranges:?}"
        );
    }
    assert_eq!(c.lines("lib/a.rb", &[]), Some(fc(0, 0)));
    assert_eq!(c.lines("lib/a.rb", &[(1, 200)]), c.file("lib/a.rb"));
    assert_eq!(
        c.uncovered_lines("lib/a.rb", &[(1, 12)]),
        Some(vec![1, 5, 7, 11])
    );
}

#[test]
fn path_mapping_ci_root_and_namesakes() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    // Produced in a CI checkout at /builds/org/repo. `ee/app/models/user.rb`
    // must not be confused with `app/models/user.rb`; `app/models/gone.rb`
    // was deleted since; `/other/vendor/x.rb` is nothing of ours.
    let rs = r#"{"RSpec": {"coverage": {
        "/builds/org/repo/app/models/user.rb": [1],
        "/builds/org/repo/ee/app/models/user.rb": [0],
        "/builds/org/repo/lib/a.rb": [1, 1],
        "/builds/org/repo/app/models/gone.rb": [1],
        "/other/vendor/x.rb": [1]
    }}}"#;
    write(root, "coverage/.resultset.json", rs);
    let c = load(root, &cfg());
    assert_eq!(c.file("app/models/user.rb"), Some(fc(1, 1)));
    assert_eq!(c.file("ee/app/models/user.rb"), Some(fc(0, 1)));
    assert_eq!(c.file("lib/a.rb"), Some(fc(2, 2)));
    let r = &c.reports()[0];
    assert_eq!((r.entries, r.mapped, r.unmapped), (5, 3, 2));
    assert_eq!(c.unmapped(), 2);
    let mut sample = c.unmapped_sample().to_vec();
    sample.sort();
    assert_eq!(
        sample,
        ["/builds/org/repo/app/models/gone.rb", "/other/vendor/x.rb"]
    );
}

#[test]
fn path_mapping_deleted_ee_file_does_not_map_to_namesake() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let known = ["app/models/user.rb", "lib/a.rb", "lib/b.rb"];
    let rs = r#"{"RSpec": {"coverage": {
        "/ci/repo/ee/app/models/user.rb": [1],
        "/ci/repo/lib/a.rb": [1],
        "/ci/repo/lib/b.rb": [1]
    }}}"#;
    write(root, "coverage/.resultset.json", rs);
    let c = Coverage::load(root, &cfg(), known.iter().copied()).unwrap();
    assert_eq!(
        c.file("app/models/user.rb"),
        None,
        "learned root /ci/repo: ee/... is gone, not app/..."
    );
    assert_eq!(c.unmapped(), 1);
}

#[test]
fn path_mapping_relative_package_paths() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    // Jest run in packages/web writes paths relative to it.
    write(
        root,
        "packages/web/coverage/lcov.info",
        "SF:src/foo.js\nDA:1,1\nend_of_record\nSF:./src/bar.js\nDA:1,0\nend_of_record\n",
    );
    let c = load(
        root,
        &CoverageConfig {
            paths: vec!["packages/web/coverage".into()],
            ..cfg()
        },
    );
    assert_eq!(c.file("packages/web/src/foo.js"), Some(fc(1, 1)));
    assert_eq!(c.file("packages/api/src/foo.js"), None);
    assert_eq!(c.file("packages/web/src/bar.js"), Some(fc(0, 1)));

    // Same report somewhere unrelated: `src/foo.js` is ambiguous on its own,
    // but the unique `src/bar.js` teaches the `packages/web` prefix.
    let d2 = tempfile::tempdir().unwrap();
    write(
        d2.path(),
        "reports/lcov.info",
        "SF:src/foo.js\nDA:1,1\nend_of_record\nSF:src/bar.js\nDA:1,0\nend_of_record\n",
    );
    let c = load(
        d2.path(),
        &CoverageConfig {
            paths: vec!["reports/lcov.info".into()],
            ..cfg()
        },
    );
    assert_eq!(c.file("packages/web/src/foo.js"), Some(fc(1, 1)));

    // Only the ambiguous one: counted as such.
    let d3 = tempfile::tempdir().unwrap();
    write(
        d3.path(),
        "reports/lcov.info",
        "SF:src/foo.js\nDA:1,1\nend_of_record\n",
    );
    let c = load(
        d3.path(),
        &CoverageConfig {
            paths: vec!["reports/lcov.info".into()],
            ..cfg()
        },
    );
    assert_eq!((c.reports()[0].unmapped, c.reports()[0].ambiguous), (1, 1));
    assert!(c.is_empty());
}

#[test]
fn no_known_files_trusts_paths_under_root() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/lcov.info",
        &format!(
            "SF:{}/x/y.rb\nDA:1,1\nend_of_record\nSF:/elsewhere/z.rb\nDA:1,1\nend_of_record\nSF:rel.js\nDA:1,0\nend_of_record\n",
            root.display()
        ),
    );
    let c = Coverage::load(root, &cfg(), std::iter::empty()).unwrap();
    assert_eq!(c.file("x/y.rb"), Some(fc(1, 1)));
    assert_eq!(c.file("rel.js"), Some(fc(0, 1)));
    assert_eq!(c.unmapped(), 1);
}

#[test]
fn merges_across_formats() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/.resultset.json",
        r#"{"A": {"coverage": {"lib/a.rb": [0, 0, null]}}}"#,
    );
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:2,3\nDA:3,0\nend_of_record\n",
    );
    let c = load(root, &cfg());
    assert_eq!(c.file("lib/a.rb"), Some(fc(1, 3)));
}

#[test]
fn malformed_inputs_never_panic() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let r = root.display();
    // A bad entry is skipped; the rest is kept.
    write(
        root,
        "coverage/.resultset.json",
        &format!(
            r#"{{"A": {{"coverage": {{"{r}/lib/a.rb": null, "{r}/app/models/user.rb": {{"lines": [1, {{"x": 1}}, [2], true, -1, 1.5]}}, "{r}/app/javascript/x.js": 7}}}}}}"#
        ),
    );
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:x,1\nDA:1\nDA:0,1\nDA:99999999,1\nDA:2,1\nend_of_record\nDA:3,1\n",
    );
    let c = load(root, &cfg());
    assert_eq!(
        c.file("app/models/user.rb"),
        Some(fc(2, 3)),
        "1 and 1.5 covered, -1 coverable"
    );
    assert_eq!(c.file("lib/a.rb"), Some(fc(1, 1)));
    assert!(c.reports().iter().all(|r| r.error.is_none()));
    assert_eq!(c.reports().iter().map(|r| r.malformed).sum::<u32>(), 2 + 5);

    // Whole-file garbage: an error per report; load fails only if all fail.
    for (name, body) in [
        (
            "coverage/.resultset.json",
            "{\"A\": {\"coverage\": {\"x\": [1, 2",
        ),
        ("coverage/.resultset.json", "[1, 2, 3]"),
        ("coverage/.resultset.json", "{\"A\": 5}"),
        ("coverage/.resultset.json", ""),
        ("coverage/lcov.info", "\u{0}\u{1}binary junk"),
        ("coverage/coverage-final.json", "{\"a\": "),
        ("coverage/coverage-final.json", "[]"),
    ] {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), name, body);
        assert!(
            Coverage::load(d.path(), &cfg(), KNOWN.iter().copied()).is_err(),
            "{name}: {body:?}"
        );
        write(
            d.path(),
            "coverage/lcov/ok.lcov",
            "SF:lib/a.rb\nDA:1,1\nend_of_record\n",
        );
        let c = Coverage::load(d.path(), &cfg(), KNOWN.iter().copied()).unwrap();
        assert_eq!(c.file("lib/a.rb"), Some(fc(1, 1)));
        assert_eq!(c.reports().iter().filter(|r| r.error.is_some()).count(), 1);
    }
    // An empty lcov file is a report with no entries.
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "coverage/lcov.info", "");
    assert!(
        Coverage::load(d.path(), &cfg(), KNOWN.iter().copied())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn no_reports_and_disabled() {
    let d = tempfile::tempdir().unwrap();
    let c = load(d.path(), &cfg());
    assert!(c.is_empty() && c.reports().is_empty());
    assert_eq!(c.freshness(None, Some(1), 2), None);
    write(
        d.path(),
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:1,1\nend_of_record\n",
    );
    assert!(c.sources_changed(d.path()), "a report appeared");
    let off = CoverageConfig {
        enabled: false,
        ..cfg()
    };
    let c = load(d.path(), &off);
    assert!(c.is_empty() && !c.sources_changed(d.path()));
}

#[test]
fn sources_changed_tracks_mtime_size_and_set() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:1,1\nend_of_record\n",
    );
    let c = load(root, &cfg());
    assert!(!c.sources_changed(root));
    write(
        root,
        "coverage/lcov.info",
        "SF:lib/a.rb\nDA:1,0\nDA:2,0\nend_of_record\n",
    );
    assert!(c.sources_changed(root), "size changed");
    let c = load(root, &cfg());
    assert!(!c.sources_changed(root));
    write(
        root,
        "coverage/lcov/extra.lcov",
        "SF:lib/a.rb\nend_of_record\n",
    );
    assert!(c.sources_changed(root), "new report");
    let c = load(root, &cfg());
    fs::remove_file(root.join("coverage/lcov/extra.lcov")).unwrap();
    assert!(c.sources_changed(root), "report removed");
    let c = load(root, &cfg());
    let f = fs::File::options()
        .write(true)
        .open(root.join("coverage/lcov.info"))
        .unwrap();
    f.set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
        .unwrap();
    assert!(c.sources_changed(root), "mtime changed");
}

#[test]
fn freshness_against_head_and_max_age() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(
        root,
        "coverage/.resultset.json",
        r#"{"A": {"coverage": {"lib/a.rb": [1]}, "timestamp": 1000000}}"#,
    );
    write(
        root,
        "coverage/lcov.info",
        "SF:app/javascript/x.js\nDA:1,1\nend_of_record\n",
    );
    let c = load(
        root,
        &CoverageConfig {
            max_age_days: Some(2),
            ..cfg()
        },
    );
    let now = now();
    let js_mtime = c
        .reports()
        .iter()
        .find(|r| r.format == Format::Lcov)
        .unwrap()
        .mtime;
    assert_eq!(c.measured_at(), Some(1000000), "oldest report");
    assert_eq!(c.newest_mtime(), Some(js_mtime.max(c.reports()[0].mtime)));
    let all = c.freshness(None, Some(1000500), now).unwrap();
    assert!(all.older_than_head && all.older_than_max_age && all.is_stale());
    let js = c
        .freshness(Some("app/javascript/x.js"), Some(1000500), now)
        .unwrap();
    assert!(!js.is_stale(), "{js:?}");
    assert_eq!(js.measured_at, js_mtime);
    let rb = c
        .freshness(Some("lib/a.rb"), None, 1000000 + 86_400)
        .unwrap();
    assert!(!rb.is_stale());
    assert_eq!(rb.age_secs, 86_400);
    assert_eq!(c.freshness(Some("unknown.rb"), None, now), None);
}

#[test]
fn heap_is_compact() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let mut s = String::new();
    let known: Vec<String> = (0..1000).map(|i| format!("app/f{i}.rb")).collect();
    for k in &known {
        s += &format!("SF:{k}\n");
        for l in 1..=100 {
            s += &format!("DA:{l},{}\n", l % 2);
        }
        s += "end_of_record\n";
    }
    write(root, "coverage/lcov.info", &s);
    let c = Coverage::load(root, &cfg(), known.iter().map(String::as_str)).unwrap();
    assert_eq!(c.file_count(), 1000);
    assert_eq!(c.totals(), fc(50_000, 100_000));
    // 2 words x 2 bitsets x 8 bytes + record + name per file.
    assert!(
        c.heap_bytes() < 1000 * (32 + 28 + 12) + 4096,
        "{}",
        c.heap_bytes()
    );
    assert_eq!(c.iter().count(), 1000);
}

#[test]
fn config_parses_from_json() {
    let c: ripple_core::Config = serde_json::from_str(
        r#"{"coverage": {"paths": ["coverage", "web/coverage/lcov.info"], "max_age_days": 7}}"#,
    )
    .unwrap();
    assert_eq!(
        c.coverage,
        CoverageConfig {
            enabled: true,
            paths: vec!["coverage".into(), "web/coverage/lcov.info".into()],
            max_age_days: Some(7)
        }
    );
    assert!(serde_json::from_str::<ripple_core::Config>(r#"{"coverage": {"path": []}}"#).is_err());
}

#[test]
fn wildcards() {
    assert!(wildcard("*.lcov", "a.lcov"));
    assert!(!wildcard("*.lcov", "a.info"));
    assert!(wildcard("cov-*-x.info", "cov-1-x.info"));
    assert!(!wildcard("a*a", "a"));
}
