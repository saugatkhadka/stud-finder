//! View/controller <-> JS mounts (`EdgeKind::ViewMount`).

pub mod resolve;
pub mod templates;

pub use resolve::{BundlerConfig, JsIndex, Outcome, Registry};
pub use templates::{MountKind, MountRef};

use ripple_core::{Confidence, RelPath};
use std::collections::{HashMap, HashSet};

/// One mount with its resolution, as last linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRecord {
    /// The Ruby-side file (template, controller, helper).
    pub file: RelPath,
    pub mount: MountRef,
    pub outcome: Outcome,
}

/// GitLab-style page bundles: `app/assets/javascripts/pages/<controller>/<action>/index.js`
/// is loaded by `webpack_controller_bundle_tags` for that controller action
/// (and `pages/<controller>/index.js` for every action of the controller).
/// Returns (ruby file, js file, label) for each controller and action template
/// that exists.
/// `actions` holds the routed `(controller, action)` pairs: a directory is an
/// action bundle only if that action is routed or has a view, so
/// `pages/projects/project_members` is not mistaken for `projects#project_members`.
pub fn page_bundles(files: &[RelPath], controllers: &HashMap<String, Vec<RelPath>>, actions: &HashSet<(&str, &str)>, file_set: &JsIndex<'_>) -> Vec<(RelPath, RelPath, String)> {
    const ROOT: &str = "app/assets/javascripts/pages/";
    let mut out = Vec::new();
    for f in files {
        let Some(i) = f.find(ROOT) else { continue };
        if !(i == 0 || f[..i].ends_with('/')) {
            continue;
        }
        let rel = &f[i + ROOT.len()..];
        let Some(dir) = rel.strip_suffix("/index.js").or_else(|| rel.strip_suffix("/index.ts")) else { continue };
        let label = format!("pages.{}", dir.replace('/', "."));
        // Whole-controller bundle.
        for c in controllers.get(dir).into_iter().flatten() {
            out.push((c.clone(), f.clone(), label.clone()));
        }
        // Action bundle; GitLab maps create -> new and update -> edit.
        if let Some((controller, action)) = dir.rsplit_once('/') {
            let routed = [action, if action == "new" { "create" } else { "" }, if action == "edit" { "update" } else { "" }]
                .iter()
                .any(|a| !a.is_empty() && actions.contains(&(controller, *a)));
            for c in controllers.get(controller).into_iter().flatten() {
                let prefix = c.find("app/controllers/").map_or("", |j| &c[..j]);
                let views: Vec<String> = ["html.haml", "html.erb", "html.slim"]
                    .iter()
                    .map(|ext| format!("{prefix}app/views/{controller}/{action}.{ext}"))
                    .filter(|v| file_set.exists(v))
                    .collect();
                if routed || !views.is_empty() {
                    out.push((c.clone(), f.clone(), label.clone()));
                }
                for v in views {
                    out.push((v, f.clone(), label.clone()));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub(crate) fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::Exact => "exact",
        Confidence::Inferred => "inferred",
        Confidence::Heuristic => "heuristic",
    }
}
