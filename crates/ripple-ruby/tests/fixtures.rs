//! Exact-output fixture test. `tests/fixtures/rails_app` is a small Rails tree
//! exercising every rule (positives and negatives); the expected edge list was
//! reviewed by hand against the source. Notable negatives, i.e. edges that
//! must NOT appear:
//! - `app/models/admin.rb` from `Admin::Profile` / `class Admin::Note`
//!   (namespace pass-through and definition-header prefixes);
//! - `lib/active_record/batches.rb` from `ActiveRecord::Base` (gem namespace
//!   reopened by the app; prefix fallback is canonical-file only);
//! - any `lib/*_ext.rb` from `Shared` (over the limit, canonical file wins);
//! - any `lib/tasks/reopen_*.rb` from `Reopened` (over the limit, no canonical file);
//! - polymorphic `owner`, dynamic `class_name: thing_class_name`;
//! - constants inside ERB/HAML/Slim comments; `require "json"` (gem);
//! - unresolvable renders (`missing_template`, `nowhere`).
//!
//! `@action` marks an edge's `from_scope`. `purchases_controller.rb` covers
//! the scoping rules: the same constant in two actions (two edges), in an
//! action and a private method (unscoped only), `before_action` blocks,
//! `def self.x`, `private def x`, implicit views; the concern
//! `purchase_lookup.rb` stays unscoped.

use ripple_core::{ChangeSet, Config, EdgeKind, ExtractOutput, Extractor, Workspace};
use ripple_ruby::RubyExtractor;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn list(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(root.join(&rel))
            .expect("read fixture dir")
            .flatten()
        {
            let child = rel.join(e.file_name());
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(child);
            } else {
                out.push(child.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

fn workspace(root: &Path) -> Workspace {
    Workspace {
        root: root.to_path_buf(),
        files: list(root),
        config: Config::default(),
    }
}

fn edges(out: &ExtractOutput) -> Vec<String> {
    let mut v: Vec<String> = out
        .edges
        .iter()
        .map(|e| {
            format!(
                "{} -> {} [{:?} {:?} L{}{}] {}",
                e.from,
                e.to,
                e.kind,
                e.confidence,
                e.line,
                e.from_scope
                    .as_deref()
                    .map(|s| format!(" @{s}"))
                    .unwrap_or_default(),
                e.detail.as_deref().unwrap_or("")
            )
        })
        .collect();
    v.sort();
    v
}

const EXPECTED: &[&str] = &[
    "Rakefile -> lib/sanitizer.rb [RubyRequire Exact L1] lib/sanitizer",
    "app/components/badge_component.html.erb -> app/models/account.rb [RailsView Inferred L1] Account",
    "app/components/badge_component.rb -> app/components/badge_component.html.erb [RailsView Inferred L0] component template",
    "app/controllers/accounts_controller.rb -> app/controllers/application_controller.rb [RubyConstant Exact L1] ApplicationController",
    "app/controllers/accounts_controller.rb -> app/models/account.rb [RubyConstant Exact L5 @show] Account",
    "app/controllers/accounts_controller.rb -> app/views/accounts/_card.html.haml [RailsView Inferred L0] views of accounts",
    "app/controllers/accounts_controller.rb -> app/views/accounts/index.json.jbuilder [RailsView Inferred L0 @index] views of accounts",
    "app/controllers/accounts_controller.rb -> app/views/accounts/show.html.erb [RailsView Inferred L0 @show] views of accounts",
    "app/controllers/accounts_controller.rb -> app/views/layouts/accounts.html.erb [RailsView Inferred L0] views of accounts",
    "app/controllers/accounts_controller.rb -> app/views/layouts/admin.html.erb [RailsView Inferred L2] admin",
    "app/controllers/accounts_controller.rb -> app/views/shared/_nav.html.erb [RailsView Inferred L6 @show] shared/nav",
    "app/controllers/accounts_controller.rb -> ee/app/views/shared/_nav.html.haml [RailsView Inferred L6 @show] shared/nav",
    "app/controllers/admin/notes_controller.rb -> app/controllers/application_controller.rb [RubyConstant Exact L2] ApplicationController",
    "app/controllers/admin/notes_controller.rb -> app/models/admin.rb [RubyConstant Exact L9 @lookup] Admin",
    "app/controllers/admin/notes_controller.rb -> app/models/admin/note.rb [RubyConstant Exact L4 @index] Admin::Note",
    "app/controllers/admin/notes_controller.rb -> app/models/admin/profile.rb [RubyConstant Exact L5 @index] Admin::Profile",
    "app/controllers/admin/notes_controller.rb -> app/models/admin/profile.rb [RubyConstant Exact L9 @lookup] Admin",
    "app/controllers/admin/notes_controller.rb -> app/models/person.rb [RailsInferred Inferred L9 @lookup] Person",
    "app/controllers/admin/notes_controller.rb -> lib/tag_parser.rb [RailsInferred Inferred L10 @lookup] TagParser",
    "app/controllers/application_controller.rb -> app/views/application/_flash.html.slim [RailsView Inferred L0] views of application",
    "app/controllers/concerns/purchase_lookup.rb -> app/models/account.rb [RubyConstant Exact L3] Account",
    "app/controllers/purchases_controller.rb -> app/controllers/application_controller.rb [RubyConstant Exact L1] ApplicationController",
    "app/controllers/purchases_controller.rb -> app/models/account.rb [RubyConstant Exact L10 @show] Account",
    "app/controllers/purchases_controller.rb -> app/models/account.rb [RubyConstant Exact L6 @index] Account",
    "app/controllers/purchases_controller.rb -> app/models/admin/note.rb [RubyConstant Exact L20] Admin::Note",
    "app/controllers/purchases_controller.rb -> app/models/admin/profile.rb [RubyConstant Exact L24] Admin::Profile",
    "app/controllers/purchases_controller.rb -> app/models/person.rb [RubyConstant Exact L30] Person",
    "app/controllers/purchases_controller.rb -> app/models/status.rb [RubyConstant Exact L3] Status",
    "app/controllers/purchases_controller.rb -> app/views/purchases/_row.html.erb [RailsView Inferred L0] views of purchases",
    "app/controllers/purchases_controller.rb -> app/views/purchases/index.html.erb [RailsView Inferred L0 @index] views of purchases",
    "app/controllers/purchases_controller.rb -> app/views/purchases/show.html.erb [RailsView Inferred L0 @show] views of purchases",
    "app/controllers/purchases_controller.rb -> app/views/shared/_nav.html.erb [RailsView Inferred L11 @show] shared/nav",
    "app/controllers/purchases_controller.rb -> ee/app/views/shared/_nav.html.haml [RailsView Inferred L11 @show] shared/nav",
    "app/controllers/purchases_controller.rb -> lib/tag_parser.rb [RailsInferred Inferred L16 @edit] TagParser",
    "app/mailers/user_mailer.rb -> app/views/user_mailer/welcome.html.erb [RailsView Inferred L0] views of user_mailer",
    "app/models/account.rb -> app/models/admin/profile.rb [RailsInferred Inferred L9] Admin::Profile",
    "app/models/account.rb -> app/models/application_record.rb [RubyConstant Exact L5] ApplicationRecord",
    "app/models/account.rb -> app/models/concerns/searchable.rb [RubyConstant Exact L6] Searchable",
    "app/models/account.rb -> app/models/media_attachment.rb [RailsInferred Inferred L23] MediaAttachment",
    "app/models/account.rb -> app/models/media_attachment.rb [RubyConstant Inferred L19] MediaAttachment",
    "app/models/account.rb -> app/models/shared.rb [RailsInferred Inferred L24] Shared",
    "app/models/account.rb -> app/models/status.rb [RailsInferred Inferred L8] Status",
    "app/models/account.rb -> app/models/status.rb [RubyConstant Exact L15] Status::VISIBILITY",
    "app/models/account.rb -> lib/sanitizer.rb [RubyRequire Exact L1] ../../lib/sanitizer",
    "app/models/account.rb -> lib/tag_parser.rb [RubyRequire Exact L2] tag_parser",
    "app/models/admin/note.rb -> app/models/admin/profile.rb [RailsInferred Inferred L4] Admin::Profile",
    "app/models/admin/note.rb -> app/models/application_record.rb [RubyConstant Exact L2] ApplicationRecord",
    "app/models/admin/note.rb -> app/models/status.rb [RailsInferred Inferred L3] Status",
    "app/models/admin/profile.rb -> app/models/account.rb [RailsInferred Inferred L3] Account",
    "app/models/admin/profile.rb -> app/models/application_record.rb [RubyConstant Exact L2] ApplicationRecord",
    "app/models/admin/profile.rb -> app/models/person.rb [RailsInferred Inferred L4] Person",
    "app/models/media_attachment.rb -> app/models/application_record.rb [RubyConstant Exact L1] ApplicationRecord",
    "app/models/media_attachment.rb -> app/models/status.rb [RailsInferred Inferred L2] Status",
    "app/models/person.rb -> app/models/application_record.rb [RubyConstant Exact L1] ApplicationRecord",
    "app/models/status.rb -> app/models/account.rb [RailsInferred Inferred L3] Account",
    "app/models/status.rb -> app/models/application_record.rb [RubyConstant Exact L1] ApplicationRecord",
    "app/models/status.rb -> app/models/media_attachment.rb [RailsInferred Inferred L4] MediaAttachment",
    "app/serializers/status_serializer.rb -> app/models/media_attachment.rb [RailsInferred Inferred L2] MediaAttachment",
    "app/views/accounts/_card.html.haml -> app/models/account.rb [RailsView Inferred L2] Account",
    "app/views/accounts/_card.html.haml -> app/views/shared/_nav.html.erb [RailsView Inferred L3] shared/nav",
    "app/views/accounts/_card.html.haml -> ee/app/views/shared/_nav.html.haml [RailsView Inferred L3] shared/nav",
    "app/views/accounts/index.json.jbuilder -> app/models/status.rb [RailsView Inferred L3] Status::VISIBILITY",
    "app/views/accounts/index.json.jbuilder -> app/views/accounts/_card.html.haml [RailsView Inferred L2] accounts/card",
    "app/views/accounts/show.html.erb -> app/models/status.rb [RailsView Inferred L4] Status::VISIBILITY",
    "app/views/accounts/show.html.erb -> app/views/accounts/_card.html.haml [RailsView Inferred L2] card",
    "app/views/accounts/show.html.erb -> app/views/application/_flash.html.slim [RailsView Inferred L3] flash",
    "app/views/application/_flash.html.slim -> lib/tag_parser.rb [RailsView Inferred L2] TagParser",
    "config.ru -> lib/sanitizer.rb [RubyConstant Exact L1] Sanitizer",
    "lib/tasks/maintenance.rake -> app/models/shared.rb [RubyConstant Exact L3] Shared",
    "lib/tasks/maintenance.rake -> lib/tag_parser.rb [RubyConstant Exact L5] TagParser",
    "spec/models/account_spec.rb -> app/models/account.rb [RubyConstant Exact L3] Account",
    "spec/models/account_spec.rb -> app/models/status.rb [RubyConstant Exact L4] Status::VISIBILITY",
    "spec/models/account_spec.rb -> spec/rails_helper.rb [RubyRequire Exact L1] rails_helper",
    "spec/rails_helper.rb -> spec/spec_helper.rb [RubyRequire Exact L1] spec_helper",
];

#[test]
fn rails_app_edges() {
    let root = fixture("rails_app");
    let out = RubyExtractor::new()
        .build(&workspace(&root))
        .expect("build");
    let actual = edges(&out);
    if actual != EXPECTED {
        for l in &actual {
            println!("            {l:?},");
        }
    }
    assert_eq!(actual, EXPECTED);
}

#[test]
fn rails_app_stats_and_warnings() {
    let root = fixture("rails_app");
    let out = RubyExtractor::new()
        .build(&workspace(&root))
        .expect("build");
    let codes: Vec<(&str, Option<&str>)> = out
        .warnings
        .iter()
        .map(|w| (w.code.as_str(), w.file.as_deref()))
        .collect();
    assert_eq!(codes, vec![("ruby_parse_error", Some("db/broken.rb"))]);
    let s = &out.stats;
    println!("{s:#?}");
    assert_eq!(s["ruby_parse_failures"], 1);
    assert_eq!(s["ruby_assoc_polymorphic_skipped"], 1);
    assert_eq!(s["ruby_assoc_dynamic_skipped"], 1);
    assert_eq!(s["ruby_namespaces_over_limit_canonical"], 1, "Shared");
    assert_eq!(s["ruby_namespaces_over_limit_no_canonical"], 1, "Reopened");
    assert_eq!(s["ruby_refs_to_namespace_without_canonical"], 1);
    assert_eq!(s["ruby_renders_unresolved"], 2);
    assert!(s["ruby_refs_namespace_skipped"] >= 3);
    for k in [
        "ruby_time_analyse_us",
        "ruby_time_merge_us",
        "ruby_time_resolve_us",
        "ruby_time_edges_us",
    ] {
        assert!(s.contains_key(k), "{k}");
    }
    assert_eq!(
        s["ruby_edges_ruby_constant"]
            + s["ruby_edges_ruby_require"]
            + s["ruby_edges_rails_inferred"]
            + s["ruby_edges_rails_view"],
        out.edges.len() as u64
    );
}

/// Removing a file through `update` must equal a fresh build without it.
#[test]
fn update_matches_build_on_fixture() {
    let root = fixture("rails_app");
    let full = workspace(&root);
    let mut ex = RubyExtractor::new();
    ex.build(&full).expect("build");
    let mut smaller = workspace(&root);
    for gone in [
        "app/models/status.rb",
        "app/views/shared/_nav.html.erb",
        "lib/tag_parser.rb",
    ] {
        smaller.files.retain(|f| f != gone);
    }
    let removed = vec![
        "app/models/status.rb".into(),
        "app/views/shared/_nav.html.erb".into(),
        "lib/tag_parser.rb".into(),
    ];
    let updated = ex
        .update(
            &smaller,
            &ChangeSet {
                modified: vec![],
                removed,
            },
        )
        .expect("update");
    let fresh = RubyExtractor::new().build(&smaller).expect("build");
    assert_eq!(edges(&updated), edges(&fresh));
    assert!(
        edges(&updated).len()
            < edges(&ex.update(&full, &ChangeSet::default()).expect("update")).len()
    );
}

#[test]
fn disabled_config_emits_nothing() {
    let root = fixture("rails_app");
    let mut ws = workspace(&root);
    ws.config.ruby.enabled = false;
    assert!(
        RubyExtractor::new()
            .build(&ws)
            .expect("build")
            .edges
            .is_empty()
    );
}

#[test]
fn controller_scopes() {
    let root = fixture("rails_app");
    let out = RubyExtractor::new()
        .build(&workspace(&root))
        .expect("build");
    for e in &out.edges {
        if e.from_scope.is_some() {
            assert!(
                e.from.starts_with("app/controllers/") && !e.from.contains("/concerns/"),
                "scoped edge from a non-controller: {e:?}"
            );
        }
    }
    let scopes = |to: &str, kind: EdgeKind| {
        let mut v: Vec<Option<String>> = out
            .edges
            .iter()
            .filter(|e| e.from == "app/controllers/purchases_controller.rb" && e.to == to && e.kind == kind)
            .map(|e| e.from_scope.clone())
            .collect();
        v.sort();
        v
    };
    let s = |x: &str| Some(x.to_string());
    let c = EdgeKind::RubyConstant;
    // In two actions: one edge per action.
    assert_eq!(scopes("app/models/account.rb", c), vec![s("index"), s("show")]);
    // In `edit` and in the private `load_account`: whole file.
    assert_eq!(scopes("app/models/person.rb", c), vec![None]);
    // `before_action { ... }`, `def self.export`, `private def secret`, class header.
    for to in [
        "app/models/status.rb",
        "app/models/admin/note.rb",
        "app/models/admin/profile.rb",
        "app/controllers/application_controller.rb",
    ] {
        assert_eq!(scopes(to, c), vec![None], "{to}");
    }
    assert_eq!(scopes("lib/tag_parser.rb", EdgeKind::RailsInferred), vec![s("edit")]);
    let v = EdgeKind::RailsView;
    assert_eq!(scopes("app/views/purchases/index.html.erb", v), vec![s("index")]);
    assert_eq!(scopes("app/views/purchases/_row.html.erb", v), vec![None]);
    assert_eq!(scopes("app/views/shared/_nav.html.erb", v), vec![s("show")]);
    assert!(
        out.edges
            .iter()
            .filter(|e| e.from == "app/controllers/concerns/purchase_lookup.rb")
            .all(|e| e.from_scope.is_none())
    );
}

fn copy_dir(src: &Path, dst: &Path) {
    if dst.exists() {
        std::fs::remove_dir_all(dst).expect("clean");
    }
    for f in list(src) {
        let to = dst.join(&f);
        std::fs::create_dir_all(to.parent().expect("parent")).expect("mkdir");
        std::fs::copy(src.join(&f), to).expect("copy");
    }
}

/// Editing a controller (moving code between actions, class body and
/// private section) through `update` must equal a fresh build, scopes included.
#[test]
fn controller_edit_update_matches_build() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ripple-ruby-scopes");
    copy_dir(&fixture("rails_app"), &root);
    let ctrl = "app/controllers/purchases_controller.rb";
    let mut ex = RubyExtractor::new();
    let before = edges(&ex.build(&workspace(&root)).expect("build"));
    let src = std::fs::read_to_string(root.join(ctrl)).expect("read");
    let edits = [
        // `edit` becomes private; `index` gains a reference; `Status` moves into `show`.
        src.replace("  def edit\n", "  private\n\n  def edit\n")
            .replace("    @accounts = Account.all\n", "    @accounts = Account.all\n    Admin::Note.count\n")
            .replace("  before_action { Status.count }\n", "")
            .replace("    render partial:", "    Status.count\n    render partial:"),
        // `show` is now invoked by `index`, so it is no longer an action scope.
        src.replace("    @accounts = Account.all\n", "    @accounts = Account.all\n    show\n"),
        src.clone(),
    ];
    assert!(edits[0].matches("Status.count").count() == 1 && edits[0].contains("private\n\n  def edit"));
    assert!(edits[1] != src);
    let mut scoped_show = Vec::new();
    for edit in edits {
        std::fs::write(root.join(ctrl), &edit).expect("write");
        let ws = workspace(&root);
        let updated = edges(
            &ex.update(&ws, &ChangeSet { modified: vec![ctrl.into()], removed: vec![] })
                .expect("update"),
        );
        let fresh = edges(&RubyExtractor::new().build(&ws).expect("build"));
        assert_eq!(updated, fresh);
        scoped_show.push(updated.iter().any(|l| l.starts_with(ctrl) && l.contains("@show")));
    }
    assert_eq!(scoped_show, vec![true, false, true]);
    assert_eq!(
        edges(&ex.update(&workspace(&root), &ChangeSet::default()).expect("update")),
        before
    );
}
