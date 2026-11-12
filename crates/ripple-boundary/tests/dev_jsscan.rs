//! Tests for the DEV-ONLY regex call-site scanner used by `examples/dump`.
//! (The production extractor is the JS crate; this only feeds dev tooling.)

#[path = "../examples/support/jsscan.rs"]
mod jsscan;

#[test]
fn scans_common_client_forms() {
    let src = r#"
api().get(`/api/v1/accounts/${id}`).then(r => r);
api(getState).post(
  '/api/v1/statuses',
  params,
);
apiRequestGet<Thing>('v1/profile');
apiRequest('PUT', `v1/lists/${id}`);
fetch("/api/v2/widgets?x=1", { method: "DELETE" });
fetch("https://example.com/feed.json");
request({ method: "POST", accept: "json", url: Routes.link_path(id) });
request({ method: "GET", url: "/github_stars" });
cache.put('/', root);
app.get('/api/v1/streaming/*splat', (req, res) => {});
map.get('/x');
"#;
    let got: Vec<(u32, Option<String>, String, bool)> = jsscan::scan("a.ts", src).into_iter().map(|s| (s.line, s.method, s.url, s.route_helper)).collect();
    let want: Vec<(u32, Option<String>, String, bool)> = vec![
        (2, Some("GET".into()), "/api/v1/accounts/:param".into(), false),
        (3, Some("POST".into()), "/api/v1/statuses".into(), false),
        (7, Some("GET".into()), "v1/profile".into(), false),
        (8, Some("PUT".into()), "v1/lists/:param".into(), false),
        (9, Some("DELETE".into()), "/api/v2/widgets".into(), false),
        (10, Some("GET".into()), "/feed.json".into(), false),
        (11, Some("POST".into()), "Routes.link_path".into(), true),
        (12, Some("GET".into()), "/github_stars".into(), false),
    ];
    assert_eq!(got, want);
}
