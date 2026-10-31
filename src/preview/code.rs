use crate::preview::PreviewContent;
use eframe::egui::Color32;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Cap text preview at 128 KB to keep RAM usage low
const MAX_TEXT_BYTES: usize = 128 * 1024;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

pub fn load_code_or_text(path: &Path, ext: &str) -> PreviewContent {
    let Ok(file) = File::open(path) else {
        return PreviewContent::Unsupported {
            reason: "Failed to open file.".to_string(),
            hex_preview: String::new(),
        };
    };

    let total_len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let truncated = total_len > MAX_TEXT_BYTES;

    let mut buf = Vec::with_capacity(total_len.min(MAX_TEXT_BYTES));
    if file.take(MAX_TEXT_BYTES as u64).read_to_end(&mut buf).is_err() {
        return PreviewContent::Unsupported {
            reason: "Error reading file.".to_string(),
            hex_preview: String::new(),
        };
    }

    let mut raw_text = String::from_utf8_lossy(&buf).into_owned();

    // Pretty-print minified JSON files for readability
    if ext == "json" && !truncated {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw_text) {
            if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                raw_text = pretty;
            }
        }
    }

    let is_markdown = matches!(ext, "md" | "markdown");

    let ss = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);

    let syntax = ss
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .or_else(|| ss.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let theme = ts
        .themes
        .get("base16-ocean.dark")
        .or_else(|| ts.themes.values().next())
        .expect("Default theme must be available");

    let mut h = HighlightLines::new(syntax, theme);
    let mut highlighted_sections = Vec::new();

    for (line_num, line) in LinesWithEndings::from(&raw_text).enumerate() {
        // Line number column
        highlighted_sections.push((
            Color32::from_rgb(95, 105, 118),
            Color32::TRANSPARENT,
            format!("{:>4} │ ", line_num + 1),
        ));

        match h.highlight_line(line, ss) {
            Ok(ranges) => {
                for (style, piece) in ranges {
                    let fg = Color32::from_rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    );
                    highlighted_sections.push((fg, Color32::TRANSPARENT, piece.to_string()));
                }
            }
            Err(_) => {
                highlighted_sections.push((
                    Color32::from_rgb(220, 224, 230),
                    Color32::TRANSPARENT,
                    line.to_string(),
                ));
            }
        }
    }

    PreviewContent::Code {
        highlighted_sections,
        raw_text,
        syntax_name: syntax.name.clone(),
        truncated,
        is_markdown,
    }
}
