//! Exact-output fixture tests. Each fixture is a small tree under `tests/fixtures/`;
//! the expected edge / call-site lists were reviewed by hand against the source.
mod common;
use common::*;
use ripple_core::{Config, Extractor};
use ripple_js::JsExtractor;

fn run(name: &str, config: Config) -> ripple_core::ExtractOutput {
    let root = fixture(name);
    JsExtractor::new().build(&workspace(&root, config)).unwrap()
}

fn assert_lines(label: &str, actual: Vec<String>, expected: &[&str]) {
    let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    if actual != expected {
        show(label, &actual);
    }
    assert_eq!(actual, expected, "{label}");
}

#[test]
fn every_import_form() {
    let out = run("imports", Config::default());
    assert_lines(
        "edges",
        edges(&out),
        &[
            "src/main.ts -> src/assets/config.yml [Asset Exact L22] ./assets/config.yml",
            "src/main.ts -> src/assets/data.json [Asset Exact L20] ./assets/data.json",
            "src/main.ts -> src/assets/icon.svg [Asset Exact L19] ./assets/icon.svg?react",
            "src/main.ts -> src/assets/loader.scss [Asset Exact L23] style-loader!./assets/loader.scss",
            "src/main.ts -> src/assets/query.graphql [Asset Exact L21] ./assets/query.graphql",
            "src/main.ts -> src/assets/style.css [Asset Exact L18] ./assets/style.css",
            "src/main.ts -> src/ctx/a.js [JsDynamicImport Inferred L32] ./ctx",
            "src/main.ts -> src/ctx/b.js [JsDynamicImport Inferred L32] ./ctx",
            "src/main.ts -> src/ctx/c.jsx [JsDynamicImport Inferred L33] ./ctx",
            "src/main.ts -> src/ctx/deep/d.js [JsDynamicImport Inferred L33] ./ctx",
            "src/main.ts -> src/ctx/deep/e.jsx [JsDynamicImport Inferred L33] ./ctx",
            "src/main.ts -> src/lib/a.ts [JsImport Exact L1] ./lib/a",
            "src/main.ts -> src/lib/b.ts [JsImport Exact L2] ./lib/b",
            "src/main.ts -> src/lib/c.ts [JsImport Exact L3] ./lib/c",
            "src/main.ts -> src/lib/cjs.ts [JsImport Exact L12] ./lib/cjs",
            "src/main.ts -> src/lib/dir/index.ts [JsImport Exact L15] ./lib/dir",
            "src/main.ts -> src/lib/dynamic.ts [JsDynamicImport Exact L14] ./lib/dynamic",
            "src/main.ts -> src/lib/eq-type.ts [JsImport Exact L28] ./lib/eq-type",
            "src/main.ts -> src/lib/eq.ts [JsImport Exact L11] ./lib/eq",
            "src/main.ts -> src/lib/lazy.ts [JsDynamicImport Exact L13] ./lib/lazy",
            "src/main.ts -> src/lib/mixed.ts [JsImport Exact L7] ./lib/mixed",
            "src/main.ts -> src/lib/reexport-all.ts [JsImport Exact L8] ./lib/reexport-all",
            "src/main.ts -> src/lib/reexport-named.ts [JsImport Exact L9] ./lib/reexport-named",
            "src/main.ts -> src/lib/reexport-type.ts [JsImport Exact L10] ./lib/reexport-type (type)",
            "src/main.ts -> src/lib/side-effect.ts [JsImport Exact L4] ./lib/side-effect",
            "src/main.ts -> src/lib/tsfile.ts [JsImport Exact L17] ./lib/tsfile.js",
            "src/main.ts -> src/lib/types-only.ts [JsImport Exact L29] ./lib/types-only (type)",
            "src/main.ts -> src/lib/types.ts [JsImport Exact L5] ./lib/types (type)",
            "src/main.ts -> src/lib/types2.ts [JsImport Exact L6] ./lib/types2 (type)",
            "src/main.ts -> src/locales/en.json [Asset Inferred L30] ./locales/*.json",
            "src/main.ts -> src/locales/fr.json [Asset Inferred L30] ./locales/*.json",
            "src/main.ts -> src/pkgdir/entry.js [JsImport Exact L16] ./pkgdir",
            "src/main.ts -> src/sub/one.ts [JsDynamicImport Inferred L31] ./sub/*.ts, !./sub/skip.ts",
            "src/main.ts -> src/sub/two.ts [JsDynamicImport Inferred L31] ./sub/*.ts, !./sub/skip.ts",
        ],
    );
    assert_lines("warnings", warnings(&out), &["js_unresolved_import src/main.ts:27: cannot resolve relative import './lib/does-not-exist'"]);
    // 30 import statements: 26 resolved, 3 externals (react, @scope/pkg/deep, node:fs), 1 unresolved.
    // The self-import (`./main`) resolves but yields no edge.
    assert_eq!(out.stats["js_imports_total"], 30);
    assert_eq!(out.stats["js_imports_resolved"], 26);
    assert_eq!(out.stats["js_imports_external"], 3);
    assert_eq!(out.stats["js_imports_unresolved"], 1);
}

#[test]
fn tsconfig_paths_extends_and_nearest() {
    let out = run("tsconfig", Config::default());
    assert_lines(
        "edges",
        edges(&out),
        &[
            // jsconfig.json baseUrl "./"
            "legacy/old.js -> legacy/helpers/h.js [JsImport Exact L1] helpers/h",
            // nearest config (packages/web) supplies @web/*
            "packages/web/src/page.ts -> packages/web/src/widget.ts [JsImport Exact L1] @web/widget",
            // web's `paths` replaced the base ones; @base/* is found via the ancestor (root) config
            "packages/web/src/page.ts -> shared/util.ts [JsImport Exact L2] @base/util",
            // `paths` + `baseUrl` inherited through `extends: "./tsconfig.base"` (no .json, JSONC)
            "src/app.ts -> shared/exact.ts [JsImport Exact L2] exact-name",
            "src/app.ts -> shared/util.ts [JsImport Exact L1] @base/util",
        ],
    );
    assert_lines("warnings", warnings(&out), &["js_unresolved_import src/app.ts:4: cannot resolve alias import '@base/nope'"]);
    // `lodash` is external even though a baseUrl exists.
    assert_eq!(out.stats["js_imports_external"], 1);
    assert_eq!(out.stats["js_tsconfigs"], 3);
}

#[test]
fn tsconfig_override_and_aliases_from_config() {
    let mut cfg = Config::default();
    cfg.js.tsconfig = vec!["packages/web/tsconfig.json".into()];
    let out = run("tsconfig", cfg);
    // Auto-detection is off: the root config and jsconfig are not read, so
    // only files under packages/web see `paths`.
    assert_lines(
        "edges",
        edges(&out),
        &["packages/web/src/page.ts -> packages/web/src/widget.ts [JsImport Exact L1] @web/widget"],
    );

    let mut cfg = Config::default();
    cfg.js.tsconfig = vec!["no/such/tsconfig.json".into()];
    cfg.js.aliases.insert("@base".into(), "shared".into());
    cfg.js.aliases.insert("@ghost".into(), "does/not/exist".into());
    let out = run("tsconfig", cfg);
    assert!(edges(&out).contains(&"src/app.ts -> shared/util.ts [JsImport Exact L1] @base/util".to_string()));
    // Bad config is reported, not swallowed.
    let w = warnings(&out).join("\n");
    assert!(w.contains("js_tsconfig_problem"), "{w}");
    assert!(w.contains("js_alias_target_missing"), "{w}");
}

#[test]
fn workspace_packages() {
    let out = run("workspace", Config::default());
    assert_lines(
        "edges",
        edges(&out),
        &[
            // `main`
            "packages/app/src/index.ts -> packages/a/lib/deep/file.ts [JsImport Exact L2] @acme/a/lib/deep/file",
            "packages/app/src/index.ts -> packages/a/lib/index.ts [JsImport Exact L1] @acme/a",
            // `exports`: subpath, and "." where the first condition (`types`) points at a missing file
            "packages/app/src/index.ts -> packages/b/src/feature/index.ts [JsImport Exact L4] @acme/b/feature",
            "packages/app/src/index.ts -> packages/b/src/main.ts [JsImport Exact L3] @acme/b",
            // exports pattern "./utils/*"
            "packages/app/src/index.ts -> packages/b/src/utils/str.ts [JsImport Exact L5] @acme/b/utils/str",
            // no main: src/index.ts
            "packages/app/src/index.ts -> packages/c/src/index.ts [JsImport Exact L6] @acme/c",
        ],
    );
    assert_lines("warnings", warnings(&out), &["js_unresolved_import packages/app/src/index.ts:7: cannot resolve workspace package import '@acme/b/nope'"]);
    assert_eq!(out.stats["js_workspace_packages"], 4);
    assert_eq!(out.stats["js_workspace_total"], 7);
    assert_eq!(out.stats["js_workspace_resolved"], 6);
    // react + @acme/not-a-workspace
    assert_eq!(out.stats["js_imports_external"], 2);
}

#[test]
fn vue_script_blocks() {
    let out = run("vue", Config::default());
    assert_lines(
        "edges",
        edges(&out),
        &[
            // `<script src>`; line = the tag's line
            "src/Bar.vue -> src/bar-impl.ts [JsImport Exact L2] ./bar-impl.ts",
            // `<script>` and `<script setup lang="ts">` in one file; lines are file lines
            "src/Foo.vue -> src/Bar.vue [JsImport Exact L11] ./Bar.vue",
            "src/Foo.vue -> src/lazy.ts [JsDynamicImport Exact L13] ./lazy",
            "src/Foo.vue -> src/types.ts [JsImport Exact L12] ./types (type)",
            "src/Foo.vue -> src/util.ts [JsImport Exact L6] ./util",
            "src/main.ts -> src/Foo.vue [JsImport Exact L1] ./Foo.vue",
        ],
    );
    assert!(out.warnings.is_empty());
}

#[test]
fn call_sites_builtin_forms() {
    let out = run("calls", Config::default());
    assert_lines(
        "calls",
        calls(&out),
        &[
            "src/calls.ts:10 DELETE /api/v1/object-form via axios",
            "src/calls.ts:11 PATCH /api/v1/direct via axios",
            "src/calls.ts:12 POST /legacy/ajax via $.ajax",
            "src/calls.ts:13 GET /legacy/get via $.get",
            "src/calls.ts:14 POST /legacy/post via $.post",
            "src/calls.ts:15 GET /legacy/json via $.getJSON",
            "src/calls.ts:16 - /api/v1/items/:param/concat via fetch",
            "src/calls.ts:17 GET /api/v1/superagent via request.get",
            "src/calls.ts:18 POST /api/v1/superagent-post via superagent.post",
            // not built-in callees: only the "/api/" literal pass sees these;
            // the verb comes from the data context (`x.get(...)`, `x.put(...)`)
            "src/calls.ts:19 GET /api/v1/chain via <literal>",
            "src/calls.ts:20 DELETE /api/v1/chain/:param via <literal>",
            "src/calls.ts:23 PUT /api/v1/dotted via <literal>",
            "src/calls.ts:24 PUT /api/v1/dotted-this via <literal>",
            // route helpers: `url` holds the helper name
            "src/calls.ts:25 - admin_user_path via Routes.admin_user_path (route helper)",
            "src/calls.ts:26 - user_path via user_path (route helper)",
            "src/calls.ts:27 - settings_url via settings_url (route helper)",
            // verb learned from the enclosing request({ method, url: Routes.x() })
            "src/calls.ts:29 DELETE sellers_switch_path via Routes.sellers_switch_path (route helper)",
            "src/calls.ts:30 GET user_path via user_path (route helper)",
            "src/calls.ts:31 - /api/v1/literal via <literal>",
            "src/calls.ts:32 - /api/v1/tpl/:param via <literal>",
            "src/calls.ts:38 - /api/v1/with space via fetch",
            "src/calls.ts:5 - /api/v1/plain via fetch",
            "src/calls.ts:6 PUT /api/v1/items/:param/edit via fetch",
            "src/calls.ts:7 - /api/v1/origin via fetch",
            "src/calls.ts:8 POST /api/v1/posted via axios.post",
            "src/calls.ts:9 GET /api/v1/based via axios.get",
        ],
    );
}

#[test]
fn call_sites_configured_callees_and_prefixes() {
    let mut cfg = Config::default();
    cfg.boundary.http_callees = ["api().get", "api().delete", "apiRequest", "apiRequestGet", "http.put"].map(String::from).to_vec();
    cfg.boundary.url_prefixes = vec!["/internal/".into()];
    let out = run("calls", cfg);
    let got = calls(&out);
    for expected in [
        "src/calls.ts:19 GET /api/v1/chain via api().get",
        "src/calls.ts:20 DELETE /api/v1/chain/:param via api().delete",
        // leading verb string, URL without leading slash kept as written
        "src/calls.ts:21 POST v1/statuses/:param/favourite via apiRequest",
        // verb from the callee's camelCase suffix
        "src/calls.ts:22 GET v1/timelines/home via apiRequestGet",
        // dotted entry matches `http.put` and `this.http.put`
        "src/calls.ts:23 PUT /api/v1/dotted via http.put",
        "src/calls.ts:24 PUT /api/v1/dotted-this via this.http.put",
        // configured url_prefixes extend the literal pass
        "src/calls.ts:33 - /internal/prefixed/thing via <literal>",
    ] {
        assert!(got.contains(&expected.to_string()), "missing {expected:?} in {got:#?}");
    }
    // A configured call must not also be reported as a bare literal.
    assert!(!got.iter().any(|c| c.starts_with("src/calls.ts:19 ") && c.ends_with("via <literal>")));
    // Strings that merely contain an API-ish path in the wrong place are not reported.
    assert!(!got.iter().any(|c| c.contains("/not-an-api/path")));
}

#[test]
fn parse_errors_are_warnings_and_still_extract() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("broken.ts"), "import a from './ok';\nconst = ;;; function (\n").unwrap();
    std::fs::write(dir.path().join("ok.ts"), "export const a = 1;\n").unwrap();
    std::fs::write(dir.path().join("binary.js"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    let out = JsExtractor::new().build(&workspace(dir.path(), Config::default())).unwrap();
    let w = warnings(&out);
    assert!(w.iter().any(|w| w.starts_with("js_parse_error broken.ts")), "{w:?}");
    assert!(edges(&out).contains(&"broken.ts -> ok.ts [JsImport Exact L1] ./ok".to_string()), "{:?}", edges(&out));
    assert_eq!(out.stats["js_parse_errors"], out.warnings.iter().filter(|w| w.code != "js_unresolved_import").count() as u64);
}

/// `file:line url class`, class = `link`, `data` or `?<context>` (unclassified).
fn classified(ex: &JsExtractor, out: &ripple_core::ExtractOutput) -> Vec<String> {
    let unclassified: Vec<(&ripple_core::JsCallSite, &str)> = ex.unclassified_call_sites();
    let mut v: Vec<String> = out
        .facts
        .js_call_sites
        .iter()
        .map(|c| {
            let class = match unclassified.iter().find(|(u, _)| *u == c) {
                Some((_, label)) => format!("?{label}"),
                None if c.link => "link".into(),
                None => "data".into(),
            };
            assert!(!(c.link && class.starts_with('?')), "unclassified site marked link: {c:?}");
            format!("{}:{} {} {}", c.file, c.line, c.url, class)
        })
        .collect();
    v.sort_by_key(|s| {
        let (file, rest) = s.split_once(':').unwrap();
        (file.to_string(), rest.split(' ').next().unwrap().parse::<u32>().unwrap(), s.clone())
    });
    v
}

#[test]
fn call_sites_navigation_vs_data() {
    let root = fixture("links");
    let mut ex = JsExtractor::new();
    let out = ex.build(&workspace(&root, Config::default())).unwrap();
    assert_lines(
        "links",
        classified(&ex, &out),
        &[
            "src/Calls.ts:6 home_path link",                // window.location =
            "src/Calls.ts:7 dashboard_path link",           // location.href =
            "src/Calls.ts:8 wallet_path link",              // window.location.href = x + "?x=1"
            "src/Calls.ts:9 assign_path link",              // location.assign
            "src/Calls.ts:10 replace_path link",            // location.replace
            "src/Calls.ts:11 open_path link",               // window.open
            "src/Calls.ts:12 visit_path link",              // Inertia router.visit
            "src/Calls.ts:13 get_path link",                // Inertia router.get (query data only)
            "src/Calls.ts:14 visit_post_path data",         // router.visit with method: post
            "src/Calls.ts:15 post_path data",               // router.post
            "src/Calls.ts:16 destroy_path data",            // router.delete
            "src/Calls.ts:18 form_patch_path data",         // useForm().patch
            "src/Calls.ts:19 fetch_path data",              // fetch
            "src/Calls.ts:20 axios_path data",              // axios.get
            "src/Calls.ts:21 wrapped_path data",            // fetch(withQuery(...))
            "src/Calls.ts:22 after_path link",              // location in a success callback
            "src/Calls.ts:22 ajax_path data",               // $.ajax({ url })
            "src/Calls.ts:23 push_state_path link",         // history.pushState
            "src/Calls.ts:24 active_path link",             // compared against location.pathname
            "src/Calls.ts:28 items_home_path link",         // { href } in a nav menu
            "src/Calls.ts:29 items_lib_path link",          // { url } in a nav menu
            "src/Calls.ts:33 var_data_path data",           // const url = ..; fetch(url)
            "src/Calls.ts:38 var_link_path link",           // const next = ..; location.href = next
            "src/Calls.ts:43 library_url link",             // new URL(..), mutated, then navigated
            "src/Calls.ts:49 state_path ?arg:useState",
            "src/Calls.ts:50 passed_path ?var:arg:doSomething",
            "src/Calls.ts:52 unused_path ?var:unused",
            "src/Calls.ts:53 returned_path ?return",
            "src/Calls.ts:56 exported_path ?export",
            "src/Calls.ts:57 endpoint_path ?prop:endpoint",
            "src/Calls.ts:58 query_path ?arg:useQuery",     // { url } passed to an unknown call
            "src/Calls.ts:59 /api/graphql data",            // { path } next to method: a request description
            "src/Calls.ts:60 client_request_path data",     // client.request({ url })
            "src/Calls.ts:62 switch_path link",             // switch-case comparison
            "src/Calls.ts:67 var_post_a_path data",         // const postUrl = a ? b : c; form.post(postUrl)
            "src/Calls.ts:67 var_post_b_path data",
            "src/Nav.tsx:7 root_url link",                  // <a href>
            "src/Nav.tsx:8 library_path link",              // <Link href>
            "src/Nav.tsx:9 settings_path link",             // <NavLink to>
            "src/Nav.tsx:10 avatar_url link",               // <img src>
            "src/Nav.tsx:11 profile_url link",              // template around the helper
            "src/Nav.tsx:12 a_path link",                   // either branch of ?:
            "src/Nav.tsx:12 b_path link",
            "src/Nav.tsx:13 logout_path data",              // <Link method="delete">
            "src/Nav.tsx:14 checkout_path data",            // <Form method="post">
            "src/Nav.tsx:15 search_path link",              // GET <form action>
            "src/Nav.tsx:16 search_api_path ?jsx:endpoint", // component prop: may be fetched
            "src/Nav.tsx:17 raw_path ?jsx-child",
            "src/Nav.tsx:18 /api/v1/docs link",             // literal href
        ],
    );
    assert_eq!(out.stats["js_call_sites"], 49);
    assert_eq!(out.stats["js_call_sites_link"], 25);
    assert_eq!(out.stats["js_call_sites_unclassified"], 9);
    // Data contexts that name a verb lend it to route helpers inside them.
    let verbs: Vec<String> = out
        .facts
        .js_call_sites
        .iter()
        .filter(|c| c.route_helper)
        .filter_map(|c| c.method.as_ref().map(|m| format!("{} {m}", c.url)))
        .collect();
    for expected in ["visit_post_path POST", "post_path POST", "destroy_path DELETE", "form_patch_path PATCH", "logout_path DELETE", "checkout_path POST", "var_post_a_path POST", "var_post_b_path POST"] {
        assert!(verbs.contains(&expected.to_string()), "missing {expected:?} in {verbs:?}");
    }
    // A request description names its verb.
    assert!(out.facts.js_call_sites.iter().any(|c| c.url == "/api/graphql" && c.method.as_deref() == Some("POST")));
    // `fetch(x)` names no verb and links have none.
    assert!(!verbs.iter().any(|v| v.starts_with("fetch_path ") || v.starts_with("var_data_path ") || v.starts_with("visit_path ") || v.starts_with("get_path ")), "{verbs:?}");
}

#[test]
fn aliases_from_jest_and_bundler_configs() {
    let out = run("bundler", Config::default());
    assert_lines(
        "edges",
        edges(&out),
        &[
            // webpack `alias` (ROOT_PATH read through `require('./webpack.constants')`);
            // the Jest mapper gives the same `~` and lists CE before EE for ee_else_ce.
            "app/js/main.js -> app/js/feature.js [JsImport Exact L3] ee_else_ce/feature",
            "app/js/main.js -> app/js/lib/util.js [JsImport Exact L1] ~/lib/util",
            // CE has no such file: the EE entry of the same prefix resolves it.
            "app/js/main.js -> ee/app/js/only_ee.js [JsImport Exact L4] ee_else_ce/only_ee",
            // `Object.assign(alias, { vendor: .. })`
            "app/js/main.js -> vendor/js/thing.js [JsImport Exact L2] vendor/thing",
            "config/webpack.config.js -> config/webpack.constants.js [JsImport Exact L2] ./webpack.constants",
            // vite `{ find, replacement }` and the package.json `jest` key; scoped to packages/ui
            // (app/js/main.js's `@ui/button` stays external).
            "packages/ui/src/button.test.ts -> packages/ui/src/button.ts [JsImport Exact L2] @ui/button",
            "packages/ui/src/button.test.ts -> packages/ui/test/setup.ts [JsImport Exact L1] @t/setup",
            "packages/ui/src/index.ts -> packages/ui/src/button.ts [JsImport Exact L1] @ui/button",
            "spec/frontend/main_spec.js -> app/js/main.js [JsImport Exact L1] ~/main",
            // `const rootDirEE = ..` bound inside an `if` block
            "spec/frontend/main_spec.js -> ee/app/js/only_ee.js [JsImport Exact L3] ee/only_ee",
            "spec/frontend/main_spec.js -> spec/frontend/helpers/h.js [JsImport Exact L2] jest/helpers/h",
        ],
    );
    assert_lines(
        "warnings",
        warnings(&out),
        &[
            "js_bundler_alias_unreadable config/webpack.config.js: 1 alias entry not statically readable, ignored: generated",
            "js_bundler_alias_unreadable jest.config.base.js: 1 alias entry not statically readable, ignored: ^dyn(/.*)$",
            // A discovered alias claims the specifier: a missing target is a warning, not external.
            "js_unresolved_import app/js/main.js:7: cannot resolve alias import '~/lib/missing'",
        ],
    );
    // lodash/debounce, @ui/button outside packages/ui, and the configs' own path x3, node:url, vite.
    assert_eq!(out.stats["js_imports_external"], 7);
}
