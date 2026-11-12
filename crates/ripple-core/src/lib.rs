//! ripple-core: shared model, config schema, extractor contract and the graph/query engine.
pub mod config;
pub mod extract;
pub mod graph;
pub mod mentions;
pub mod model;

pub use config::{Config, Risk};
pub use extract::{ChangeSet, ExtractOutput, Extractor, Linker, Workspace};
pub use graph::{Graph, Hit, Impact, NodeId, QueryOpts};
pub use mentions::{FileMentions, MentionTable};
pub use model::{Confidence, Edge, EdgeKind, Facts, JsCallSite, RelPath, Warning};
