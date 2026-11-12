//! `BoundaryLinker` end to end on the fixture app, with hand-made call-site
//! facts: RailsRoute, ApiCall and ViewMount edges, stats, and warm == full.

use ripple_boundary::{BoundaryLinker, CallOutcome};
use ripple_core::{ChangeSet, Confidence, Config, Edge, EdgeKind, ExtractOutput, Facts, JsCallSite, Linker, Workspace};
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app")
}

fn list_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn workspace(root: &Path) -> Workspace {
    Workspace { root: root.to_path_buf(), files: list_files(root), config: Config::default() }
}

fn site(method: Option<&str>, url: &str) -> JsCallSite {
    JsCallSite {
        file: "app/javascript/widgets/api.ts".into(),
        line: 7,
        method: method.map(str::to_string),
        url: url.into(),
        callee: "fetch".into(),
        route_helper: false,
        link: false,
    }
}

fn helper(name: &str) -> JsCallSite {
    JsCallSite { route_helper: true, callee: "Routes".into(), url: name.into(), ..site(None, "") }
}

fn edges_of(out: &ExtractOutput, kind: EdgeKind) -> Vec<&Edge> {
    out.edges.iter().filter(|e| e.kind == kind).collect()
}

#[test]
fn rails_route_edges_point_at_controllers() {
    let ws = workspace(&fixture_root());
    let mut l = BoundaryLinker::new();
    let out = l.link(&ws, &Facts::default(), None).unwrap();
    let routes = edges_of(&out, EdgeKind::RailsRoute);
    let photos = routes.iter().find(|e| e.to == "app/controllers/photos_controller.rb").expect("photos edge");
    assert_eq!(photos.from, "config/routes.rb");
    assert_eq!(photos.confidence, Confidence::Exact);
    assert_eq!(photos.detail.as_deref().unwrap(), "POST /photos/:id/like → photos#like");
    assert_eq!(photos.line, 13);
    let widgets = routes.iter().find(|e| e.to == "app/controllers/api/v2/widgets_controller.rb").expect("drawn file edge");
    assert_eq!(widgets.from, "config/routes/api/v2.rb");
    // One edge per (route file, controller file) pair.
    assert_eq!(routes.iter().filter(|e| e.to == "app/controllers/pages_controller.rb").count(), 1);
    assert_eq!(out.stats["routes.total"], l.routes().len() as u64);
    assert_eq!(out.stats["routes.uninterpreted.devise_for"], 1);
    // home, photos, profiles, comments, tags, pages, admin/users, api/v2/widgets.
    assert_eq!(out.stats["routes.controllers_resolved"], 8);
    assert!(out.stats["routes.controllers_unresolved"] > 0);
}

#[test]
fn call_sites_match_with_the_documented_confidence() {
    let ws = workspace(&fixture_root());
    let facts = Facts { scopes: Default::default(), mentions: None,
        js_call_sites: vec![
            site(Some("GET"), "/api/v2/widgets"),          // 0 static + verb + one route
            site(Some("GET"), "/api/v2/widgets/:param"),   // 1 dynamic
            helper("Routes.apiV2WidgetPath"),              // 2 route helper
            site(Some("GET"), "v2/widgets.json"),          // 3 base `/api/` + format suffix
            site(Some("GET"), "/nowhere/at/all"),          // 4 unmatched
            site(None, "/photos/:param"),                  // 5 no verb
            site(Some("GET"), "/files/a/b/c.txt"),         // 6 glob
            site(Some("GET"), "/:param"),                  // 7 many controllers
            site(Some("GET"), "/sidekiq/queues"),          // 8 mount
            site(Some("POST"), "/photos/:param/like"),     // 9 member route
            helper("Routes.nope_path"),                    // 10 unknown helper
        ],
    };
    let mut l = BoundaryLinker::new();
    let out = l.link(&ws, &facts, None).unwrap();
    let m = l.call_matches();
    let endpoints = |i: usize| m[i].routes.iter().map(|&r| l.routes()[r].endpoint()).collect::<Vec<_>>();
    assert_eq!(m[0].outcome, CallOutcome::Matched(Confidence::Inferred));
    assert_eq!(endpoints(0), ["api/v2/widgets#index"]);
    assert_eq!(m[1].outcome, CallOutcome::Matched(Confidence::Heuristic));
    assert_eq!(endpoints(1), ["api/v2/widgets#show"]);
    assert_eq!(m[2].outcome, CallOutcome::Matched(Confidence::Exact));
    assert_eq!(endpoints(2), ["api/v2/widgets#show"]);
    assert_eq!(m[3].url, "/api/v2/widgets.json");
    assert_eq!(m[3].outcome, CallOutcome::Matched(Confidence::Inferred));
    assert_eq!(endpoints(3), ["api/v2/widgets#index"]);
    assert_eq!(m[4].outcome, CallOutcome::Unmatched);
    assert_eq!(m[5].outcome, CallOutcome::Matched(Confidence::Heuristic));
    assert_eq!(endpoints(5), ["photos#show"]);
    assert_eq!(m[6].outcome, CallOutcome::Matched(Confidence::Inferred));
    assert_eq!(endpoints(6), ["files#show"]);
    assert!(matches!(m[7].outcome, CallOutcome::Ambiguous(n) if n > 3), "{:?}", m[7]);
    assert!(m[7].routes.is_empty());
    assert_eq!(m[8].outcome, CallOutcome::NonController);
    assert_eq!(endpoints(9), ["photos#like"]);
    assert_eq!(m[10].outcome, CallOutcome::Unmatched);

    let calls = edges_of(&out, EdgeKind::ApiCall);
    let e = calls.iter().find(|e| e.detail.as_deref() == Some("GET /api/v2/widgets → api/v2/widgets#index")).expect("edge");
    assert_eq!((e.from.as_str(), e.to.as_str(), e.line), ("app/javascript/widgets/api.ts", "app/controllers/api/v2/widgets_controller.rb", 7));
    // files#show has no controller file: matched but no edge.
    assert!(!calls.iter().any(|e| e.detail.as_deref().unwrap_or("").contains("files#show")));
    assert_eq!(out.stats["calls.unmatched"], 2);
    assert_eq!(out.stats["calls.ambiguous"], 1);
    assert_eq!(out.stats["calls.controller_missing"], 1);
    assert_eq!(out.warnings.iter().filter(|w| w.code == "boundary_call_unmatched").count(), 2);
}

#[test]
fn url_prefixes_filter_and_bases_apply_only_without_leading_slash() {
    let mut ws = workspace(&fixture_root());
    ws.config.boundary.url_prefixes = vec!["/api/".into()];
    let facts = Facts { scopes: Default::default(), mentions: None, js_call_sites: vec![site(Some("GET"), "/about"), site(Some("GET"), "v2/widgets"), site(Some("GET"), "/api/v2/widgets")] };
    let mut l = BoundaryLinker::new();
    l.link(&ws, &facts, None).unwrap();
    let m = l.call_matches();
    assert_eq!(m[0].outcome, CallOutcome::Filtered);
    // base "/" gives /v2/widgets (filtered), base "/api/" matches.
    assert_eq!(m[1].url, "/api/v2/widgets");
    assert_eq!(m[1].outcome, CallOutcome::Matched(Confidence::Inferred));
    assert_eq!(m[2].outcome, CallOutcome::Matched(Confidence::Inferred));
}

#[test]
fn mounts_link_both_directions() {
    let ws = workspace(&fixture_root());
    let mut l = BoundaryLinker::new();
    let out = l.link(&ws, &Facts::default(), None).unwrap();
    let mounts = edges_of(&out, EdgeKind::ViewMount);
    let has = |from: &str, to: &str, conf: Confidence, detail: &str| mounts.iter().any(|e| e.from == from && e.to == to && e.confidence == conf && e.detail.as_deref() == Some(detail));
    let view = "app/views/photos/index.html.erb";
    let layout = "app/views/layouts/application.html.haml";
    let ctrl = "app/controllers/profiles_controller.rb";
    // Inertia from a controller -> pages root.
    assert!(has(ctrl, "app/javascript/pages/Users/Show.tsx", Confidence::Inferred, "mounts Users/Show"));
    assert!(has("app/javascript/pages/Users/Show.tsx", ctrl, Confidence::Inferred, &format!("props from {ctrl}")));
    // Registered react_component -> the registered import.
    assert!(has(view, "app/javascript/components/Nav.tsx", Confidence::Inferred, "mounts Nav"));
    assert!(has("app/javascript/components/Nav.tsx", view, Confidence::Inferred, &format!("props from {view}")));
    // Pack tag via shakapacker.yml.
    assert!(has(view, "app/javascript/packs/application.js", Confidence::Exact, "mounts application"));
    // Two `Card` files: heuristic, both candidates.
    assert!(has(view, "app/javascript/widgets/Card.tsx", Confidence::Heuristic, "mounts Card"));
    assert!(has(view, "app/javascript/legacy/Card.jsx", Confidence::Heuristic, "mounts Card"));
    // HAML call and data-react-class, unique basename: inferred.
    assert!(has(layout, "app/javascript/widgets/Gallery.tsx", Confidence::Inferred, "mounts Gallery"));
    assert!(has("app/javascript/widgets/Gallery.tsx", layout, Confidence::Inferred, &format!("props from {layout}")));
    // Comments are skipped; the dynamic pack tag is counted, not linked.
    assert!(!l.mounts().iter().any(|m| m.mount.name == "Hidden" || m.mount.name == "Commented" || m.mount.name == "Commented/Out"));
    assert_eq!(out.stats["mounts.dynamic"], 1);
    assert_eq!(out.stats["mounts.found"], 8);
    assert_eq!(out.stats["mounts.resolved"], 7);
    let line = mounts.iter().find(|e| e.from == view && e.detail.as_deref() == Some("mounts Nav")).unwrap().line;
    assert_eq!(line, 2);
}

fn copy_dir(from: &Path, to: &Path) {
    for rel in list_files(from) {
        let dst = to.join(&rel);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(from.join(&rel), dst).unwrap();
    }
}

fn fingerprint(out: &ExtractOutput) -> String {
    format!("{:?}\n{:?}\n{:?}", out.edges, out.warnings, out.stats)
}

#[test]
fn warm_link_equals_full_link() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("boundary-warm");
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&fixture_root(), &dir);
    let facts = Facts { scopes: Default::default(), mentions: None, js_call_sites: vec![site(Some("GET"), "/api/v2/widgets"), site(Some("GET"), "/probe")] };
    let mut ws = workspace(&dir);
    let mut warm = BoundaryLinker::new();
    let mut prev = fingerprint(&warm.link(&ws, &facts, None).unwrap());

    let append = |rel: &str, text: &str| {
        let p = dir.join(rel);
        let mut s = std::fs::read_to_string(&p).unwrap_or_default();
        s.push_str(text);
        std::fs::write(p, s).unwrap();
    };
    type Step<'a> = (&'a str, Box<dyn Fn() -> ChangeSet + 'a>);
    let steps: Vec<Step> = vec![
        ("routes edit", Box::new(|| {
            let p = dir.join("config/routes.rb");
            let s = std::fs::read_to_string(&p).unwrap().replacen("draw do\n", "draw do\n  get \"probe\", to: \"probe#show\"\n", 1);
            std::fs::write(p, s).unwrap();
            ChangeSet { modified: vec!["config/routes.rb".into()], removed: vec![] }
        })),
        ("drawn file edit", Box::new(|| {
            append("config/routes/admin.rb", "namespace :admin do\n  resources :audits, only: :index\nend\n");
            ChangeSet { modified: vec!["config/routes/admin.rb".into()], removed: vec![] }
        })),
        ("view edit", Box::new(|| {
            append("app/views/photos/index.html.erb", "<%= react_component \"Gallery\" %>\n");
            ChangeSet { modified: vec!["app/views/photos/index.html.erb".into()], removed: vec![] }
        })),
        ("controller edit", Box::new(|| {
            append("app/controllers/photos_controller.rb", "class PhotosController\n  def index = render(inertia: \"Users/Show\")\nend\n");
            ChangeSet { modified: vec!["app/controllers/photos_controller.rb".into()], removed: vec![] }
        })),
        ("new controller", Box::new(|| {
            append("app/controllers/probe_controller.rb", "class ProbeController < ApplicationController\nend\n");
            ChangeSet { modified: vec!["app/controllers/probe_controller.rb".into()], removed: vec![] }
        })),
        ("registration edit", Box::new(|| {
            append("app/javascript/packs/application.js", "import Card from \"../widgets/Card\";\nReactOnRails.register({ Card });\n");
            ChangeSet { modified: vec!["app/javascript/packs/application.js".into()], removed: vec![] }
        })),
        ("controller removed", Box::new(|| {
            std::fs::remove_file(dir.join("app/controllers/tags_controller.rb")).unwrap();
            ChangeSet { modified: vec![], removed: vec!["app/controllers/tags_controller.rb".into()] }
        })),
    ];
    for (name, step) in steps {
        let changes = step();
        ws.files = list_files(&dir);
        let w = fingerprint(&warm.link(&ws, &facts, Some(&changes)).unwrap());
        let f = fingerprint(&BoundaryLinker::new().link(&ws, &facts, None).unwrap());
        assert_eq!(w, f, "warm != full after {name}");
        assert_ne!(w, prev, "{name} should change the output");
        prev = w;
    }
}

fn link_site(url: &str) -> JsCallSite {
    JsCallSite { file: "app/javascript/components/Nav.tsx".into(), line: 3, link: true, ..helper(url) }
}

#[test]
fn navigation_links_are_skipped_unless_enabled() {
    let mut ws = workspace(&fixture_root());
    let facts = Facts { scopes: Default::default(), mentions: None, js_call_sites: vec![link_site("Routes.photo_path"), site(Some("GET"), "/api/v2/widgets")] };
    let out = BoundaryLinker::new().link(&ws, &facts, None).unwrap();
    let calls = edges_of(&out, EdgeKind::ApiCall);
    assert!(calls.iter().all(|e| e.from == "app/javascript/widgets/api.ts"), "{calls:?}");
    assert_eq!(out.stats["calls.skipped_links"], 1);
    assert_eq!(out.stats["calls.total"], 2);

    ws.config.boundary.link_edges = true;
    let out = BoundaryLinker::new().link(&ws, &facts, None).unwrap();
    let links: Vec<_> = edges_of(&out, EdgeKind::ApiCall).into_iter().filter(|e| e.from == "app/javascript/components/Nav.tsx").collect();
    assert_eq!(links.len(), 1, "{links:?}");
    assert_eq!(links[0].confidence, Confidence::Heuristic);
    assert_eq!(links[0].detail.as_deref(), Some("link: GET /photos/:id → photos#show"));
    assert_eq!(links[0].to_scope.as_deref(), Some("show"));
    assert_eq!(out.stats["calls.skipped_links"], 0);
}

#[test]
fn api_call_edges_are_scoped_to_each_matched_action() {
    let ws = workspace(&fixture_root());
    let facts = Facts { scopes: Default::default(), mentions: None,
        js_call_sites: vec![
            helper("Routes.photos_path"),                // index (GET) and create (POST)
            site(Some("POST"), "/photos"),               // create only
            site(None, "/photos/:param/comments"),       // comments#index and comments#create
        ],
    };
    let mut l = BoundaryLinker::new();
    let out = l.link(&ws, &facts, None).unwrap();
    let scoped = |to: &str, detail_part: &str| -> Vec<(Option<String>, u32)> {
        let mut v: Vec<_> = edges_of(&out, EdgeKind::ApiCall).into_iter().filter(|e| e.to == to && e.detail.as_deref().unwrap_or("").contains(detail_part)).map(|e| (e.to_scope.clone(), e.line)).collect();
        v.sort();
        v
    };
    let photos = "app/controllers/photos_controller.rb";
    assert_eq!(scoped(photos, "photos#index"), [(Some("index".into()), 7)]);
    assert_eq!(scoped(photos, "photos#create"), [(Some("create".into()), 7), (Some("create".into()), 7)]);
    assert_eq!(l.call_matches()[0].actions.len(), 2);
    assert_eq!(l.call_matches()[1].actions.len(), 1);
    let comments = "app/controllers/comments_controller.rb";
    assert_eq!(scoped(comments, "comments#"), [(Some("create".into()), 7), (Some("index".into()), 7)]);
    // Every ApiCall edge names its action.
    assert!(edges_of(&out, EdgeKind::ApiCall).iter().all(|e| e.to_scope.is_some() && e.from_scope.is_none()));
}

#[test]
fn a_verb_the_path_does_not_serve_falls_back_to_any_verb() {
    let ws = workspace(&fixture_root());
    // `POST` with `_method: "DELETE"`, or a verb guessed from context: no
    // DELETE /photos route, so every verb of the path, heuristically.
    let facts = Facts { scopes: Default::default(), mentions: None, js_call_sites: vec![site(Some("DELETE"), "/photos"), JsCallSite { method: Some("DELETE".into()), ..helper("Routes.photos_path") }] };
    let mut l = BoundaryLinker::new();
    let out = l.link(&ws, &facts, None).unwrap();
    let mut got: Vec<_> = edges_of(&out, EdgeKind::ApiCall).into_iter().map(|e| (e.to_scope.clone().unwrap(), e.confidence)).collect();
    got.sort();
    got.dedup();
    assert_eq!(got, [("create".to_string(), Confidence::Heuristic), ("index".to_string(), Confidence::Heuristic)]);
}

#[test]
fn view_mount_edges_are_scoped_to_the_rendering_action() {
    let ws = workspace(&fixture_root());
    let out = BoundaryLinker::new().link(&ws, &Facts::default(), None).unwrap();
    let mounts = edges_of(&out, EdgeKind::ViewMount);
    let find = |from: &str, to: &str| -> Vec<(Option<&str>, Option<&str>, &str)> {
        let mut v: Vec<_> = mounts.iter().filter(|e| e.from == from && e.to == to).map(|e| (e.from_scope.as_deref(), e.to_scope.as_deref(), e.detail.as_deref().unwrap_or(""))).collect();
        v.sort();
        v
    };
    let ctrl = "app/controllers/profiles_controller.rb";
    let page = "app/javascript/pages/Users/Show.tsx";
    // `render inertia:` in public `show`, and again in a private helper (whole file).
    assert_eq!(find(ctrl, page), [(None, None, "mounts Users/Show"), (Some("show"), None, "mounts Users/Show")]);
    assert_eq!(find(page, ctrl), [(None, None, "props from app/controllers/profiles_controller.rb"), (None, Some("show"), "props from app/controllers/profiles_controller.rb")]);
    // Template mounts: template edges stay unscoped; the page also depends on
    // the action that renders the template.
    let view = "app/views/photos/index.html.erb";
    let nav = "app/javascript/components/Nav.tsx";
    assert_eq!(find(view, nav), [(None, None, "mounts Nav")]);
    assert_eq!(find(nav, view), [(None, None, "props from app/views/photos/index.html.erb")]);
    assert_eq!(find(nav, "app/controllers/photos_controller.rb"), [(None, Some("index"), "props from photos#index via Nav")]);
    // A layout belongs to no action: no controller edge.
    let gallery = "app/javascript/widgets/Gallery.tsx";
    assert!(mounts.iter().filter(|e| e.from == gallery).all(|e| e.to == "app/views/layouts/application.html.haml"));
}
