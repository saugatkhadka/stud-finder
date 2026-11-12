//! Stub; owned by the ripple-ruby implementer. Keep the public type name and `new()`.
use ripple_core::{ExtractOutput, Extractor, Workspace};

#[derive(Default)]
pub struct RubyExtractor;

impl RubyExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Extractor for RubyExtractor {
    fn name(&self) -> &'static str {
        "ruby"
    }
    fn build(&mut self, _ws: &Workspace) -> anyhow::Result<ExtractOutput> {
        Ok(ExtractOutput::default())
    }
}
