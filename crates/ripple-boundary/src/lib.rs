//! Stub; owned by the ripple-boundary implementer. Keep the public type name and `new()`.
use ripple_core::{ChangeSet, ExtractOutput, Facts, Linker, Workspace};

#[derive(Default)]
pub struct BoundaryLinker;

impl BoundaryLinker {
    pub fn new() -> Self {
        Self
    }
}

impl Linker for BoundaryLinker {
    fn name(&self) -> &'static str {
        "boundary"
    }
    fn link(&mut self, _ws: &Workspace, _facts: &Facts, _changes: Option<&ChangeSet>) -> anyhow::Result<ExtractOutput> {
        Ok(ExtractOutput::default())
    }
}
