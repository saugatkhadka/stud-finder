use crate::report::Report;

/// The report as JSON (the agent API). Never truncated by the renderer.
pub fn render(report: &Report, pretty: bool) -> String {
    let out = if pretty { serde_json::to_string_pretty(report) } else { serde_json::to_string(report) };
    // Serialising plain data structs with string keys cannot fail.
    out.unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}
