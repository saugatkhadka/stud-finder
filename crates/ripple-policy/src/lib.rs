//! ripple-policy: turns a `ripple.yml` and an impact graph into a risk report.
//!
//! * [`load`]: find, parse and validate `ripple.yml` into [`ripple_core::Config`].
//! * [`Policy`]: the config with every glob compiled once; classifies files
//!   (areas, test, global, language). [`PolicyIndex`] caches that
//!   classification for every node of a [`ripple_core::Graph`].
//! * [`evaluate`]: runs the impact query for a set of [`ChangedFile`]s and
//!   applies the rules, producing a [`Report`].
//! * [`render`]: JSON, Markdown (PR comment), terminal text and GitHub Actions output.

mod changed;
mod defaults;
mod error;
mod eval;
mod findings;
mod glob;
mod lang;
pub mod load;
mod policy;
pub mod render;
mod report;
pub mod schema;
mod signals;

pub use changed::{Change, ChangeKind, ChangedFile, DefLine, FileStatus, Hunk, WholeFileReason, classify_change, def_name, def_names, scopes_for_hunks};
pub use defaults::default_rules;
pub use error::{Issue, PolicyError};
pub use eval::{EvalOptions, evaluate, evaluate_with, path_hops, summary_line};
pub use findings::mention_line;
pub use lang::{Language, Side};
pub use load::{CONFIG_FILE_NAMES, LoadedConfig, find_config, load_config, parse_config};
pub use policy::{CompiledArea, FileClass, Policy, PolicyIndex};
pub use report::*;
pub use signals::*;
pub use glob::PathFilter;
