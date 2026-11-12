//! The route interpreter against a hand-written routes file that exercises
//! every supported DSL form. Each row was checked by hand against
//! `ActionDispatch::Routing::Mapper` semantics (Rails 7/8).

use ripple_boundary::routes::{self, RouteTable};
use std::path::Path;

fn fixture_table() -> RouteTable {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app");
    let files: Vec<(String, Vec<u8>)> = ["config/routes.rb", "config/routes/admin.rb", "config/routes/api/v2.rb"]
        .iter()
        .map(|p| (p.to_string(), std::fs::read(root.join(p)).expect("fixture")))
        .collect();
    routes::build(&files, false)
}

/// (verb, path, controller#action, name ("" = unnamed), file:line)
const EXPECTED: &[(&str, &str, &str, &str, &str)] = &[
    ("GET", "/", "home#index", "root", "config/routes.rb:2"),
    ("ANY", "/sidekiq", "mount Sidekiq::Web", "sidekiq", "config/routes.rb:5"),
    ("POST", "/photos/:id/like", "photos#like", "like_photo", "config/routes.rb:13"),
    ("GET", "/photos/search", "photos#search", "search_photos", "config/routes.rb:16"),
    ("GET", "/photos/:id/preview", "photos#preview", "preview_photo", "config/routes.rb:18"),
    ("GET", "/photos/:photo_id/tags", "tags#index", "photo_tags", "config/routes.rb:19"),
    ("GET", "/photos/:photo_id/comments", "comments#index", "photo_comments", "config/routes.rb:8"),
    ("POST", "/photos/:photo_id/comments", "comments#create", "", "config/routes.rb:8"),
    ("GET", "/photos", "photos#index", "photos", "config/routes.rb:11"),
    ("POST", "/photos", "photos#create", "", "config/routes.rb:11"),
    ("GET", "/photos/:id", "photos#show", "photo", "config/routes.rb:11"),
    ("GET", "/profile/avatar", "profiles#avatar", "avatar_profile", "config/routes.rb:23"),
    ("GET", "/profile", "profiles#show", "profile", "config/routes.rb:22"),
    ("PATCH", "/profile", "profiles#update", "", "config/routes.rb:22"),
    ("PUT", "/profile", "profiles#update", "", "config/routes.rb:22"),
    ("GET", "/posts/:slug", "blog/articles#show", "post", "config/routes.rb:26"),
    ("GET", "/boards/:board_id/cards", "cards#index", "board_cards", "config/routes.rb:29"),
    ("GET", "/cards/:id", "cards#show", "card", "config/routes.rb:29"),
    ("GET", "/boards/:id", "boards#show", "board", "config/routes.rb:28"),
    ("GET", "/admin/users", "admin/users#index", "admin_users", "config/routes.rb:33"),
    ("GET", "/admin/stats", "admin/dashboard#stats", "admin_stats", "config/routes.rb:34"),
    ("GET", "/:locale/help", "localized/pages#help", "loc_help", "config/routes.rb:38"),
    ("GET", "/login", "sessions#new", "login", "config/routes.rb:42"),
    ("DELETE", "/logout", "sessions#destroy", "logout", "config/routes.rb:43"),
    ("GET", "/about", "pages#about", "about", "config/routes.rb:46"),
    ("GET", "/legal/terms", "legal#terms", "legal_terms", "config/routes.rb:47"),
    ("POST", "/feedback", "feedback#create", "feedback", "config/routes.rb:48"),
    ("GET", "/status", "status#show", "status", "config/routes.rb:49"),
    ("HEAD", "/status", "status#show", "status", "config/routes.rb:49"),
    ("GET", "/old", "redirect", "old", "config/routes.rb:50"),
    ("GET", "/files/*path", "files#show", "", "config/routes.rb:51"),
    ("GET", "/ping", "health#ping", "ping", "config/routes.rb:55"),
    ("GET", "/reports/daily", "reports#daily", "reports_daily", "config/routes.rb:60"),
    ("GET", "/faq", "pages#faq", "faq", "config/routes.rb:64"),
    ("GET", "/contact", "pages#contact", "contact", "config/routes.rb:64"),
    ("GET", "/teams/:team_id/members/new", "members#new", "new_team_member", "config/routes.rb:69"),
    ("DELETE", "/members/:id", "members#destroy", "member", "config/routes.rb:69"),
    ("GET", "/teams/new", "teams#new", "new_team", "config/routes.rb:68"),
    ("GET", "/teams/:id/edit", "teams#edit", "edit_team", "config/routes.rb:68"),
    ("GET", "/events/:event_id/comments", "comments#index", "event_comments", "config/routes.rb:8"),
    ("POST", "/events/:event_id/comments", "comments#create", "", "config/routes.rb:8"),
    ("GET", "/events/upcoming", "events#upcoming", "upcoming_events", "config/routes.rb:75"),
    ("GET", "/v/notes", "notes#index", "versioned_notes", "config/routes.rb:79"),
    ("GET", "/v/links", "links#index", "versioned_links", "config/routes.rb:79"),
    ("ANY", "/operations/health", "ops/checks#health", "ops_health", "config/routes.rb:83"),
    ("GET", "/admin/reports", "admin/reports#index", "admin_reports", "config/routes/admin.rb:2"),
    ("GET", "/api/v2/widgets", "api/v2/widgets#index", "api_v2_widgets", "config/routes/api/v2.rb:3"),
    ("GET", "/api/v2/widgets/:id", "api/v2/widgets#show", "api_v2_widget", "config/routes/api/v2.rb:3"),
];

#[test]
fn route_table_is_exact() {
    let t = fixture_table();
    let got: Vec<(String, String, String, String, String)> = t
        .routes
        .iter()
        .map(|r| (r.verb.clone(), r.path.clone(), r.endpoint(), r.name.clone().unwrap_or_default(), format!("{}:{}", r.file, r.line)))
        .collect();
    let want: Vec<(String, String, String, String, String)> =
        EXPECTED.iter().map(|(v, p, e, n, l)| (v.to_string(), p.to_string(), e.to_string(), n.to_string(), l.to_string())).collect();
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "row {i}");
    }
    assert_eq!(got.len(), want.len());
}

#[test]
fn uninterpreted_calls_are_counted_warnings() {
    let t = fixture_table();
    assert_eq!(t.uninterpreted.get("devise_for"), Some(&1));
    assert_eq!(t.uninterpreted.get("weird_macro"), Some(&1));
    assert_eq!(t.uninterpreted.values().sum::<u64>(), 2);
    let msgs: Vec<&str> = t.warnings.iter().map(|w| w.message.as_str()).collect();
    assert!(msgs.contains(&"config/routes.rb:4: `devise_for` generates routes that are not modelled"), "{msgs:?}");
    assert!(msgs.contains(&"config/routes.rb:93: unknown routing call `weird_macro`"), "{msgs:?}");
    assert!(t.unreached.is_empty());
}

#[test]
fn constraints_and_route_kinds() {
    let t = fixture_table();
    let find = |p: &str| t.routes.iter().find(|r| r.path == p).expect(p);
    assert!(find("/ping").constrained, "constraints block marks routes constrained");
    assert!(!find("/about").constrained);
    assert_eq!(find("/old").kind, routes::RouteKind::Redirect);
    assert!(matches!(find("/sidekiq").kind, routes::RouteKind::Mount { .. }));
}

#[test]
fn garbage_never_panics() {
    let inputs: &[&[u8]] = &[
        b"Rails.application.routes.draw do\n  resources :x do\n",
        b"\xff\xfe not ruby",
        b"Rails.application.routes.draw do\n  resources\n  namespace\n  get\n  match 1, 2\n  scope(*args) { get 'x' }\n  draw\n  concerns :missing\nend\n",
        b"",
    ];
    for src in inputs {
        let t = routes::build(&[("config/routes.rb".to_string(), src.to_vec())], false);
        assert!(t.routes.len() < 10);
    }
}

#[test]
fn api_only_drops_new_and_edit() {
    let src = b"Rails.application.routes.draw do\n  resources :posts\nend\n".to_vec();
    let full = routes::build(&[("config/routes.rb".to_string(), src.clone())], false);
    let api = routes::build(&[("config/routes.rb".to_string(), src)], true);
    assert_eq!(full.routes.len(), 8);
    assert_eq!(api.routes.len(), 6);
    assert!(!api.routes.iter().any(|r| r.action == "new" || r.action == "edit"));
}
