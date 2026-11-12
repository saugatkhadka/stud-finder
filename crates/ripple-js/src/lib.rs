//! Stub; owned by the ripple-js implementer. Keep the public type name and `new()`.
use ripple_core::{ExtractOutput, Extractor, Workspace};

#[derive(Default)]
pub struct JsExtractor;

impl JsExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Extractor for JsExtractor {
    fn name(&self) -> &'static str {
        "js"
    }
    fn build(&mut self, _ws: &Workspace) -> anyhow::Result<ExtractOutput> {
        Ok(ExtractOutput::default())
    }
}
