pub struct MarkdownExporter;

impl MarkdownExporter {
    /// Exports session search results to a structured Markdown document.
    pub fn export(sessions: Vec<String>, output_path: &str) -> std::io::Result<()> {
        let mut content = String::from("# Agent Session Search Results\n\n");
        for session in sessions {
            content.push_str(&format!("## Session\n{}\n\n", session));
        }
        std::fs::write(output_path, content)
    }
}
