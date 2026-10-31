//! Loads system fonts via fontconfig.
//!
//! Instead of embedding egui's default fonts (~1.4 MB), we use KDE's already-installed
//! system fonts: this keeps the binary small and renders the window with Plasma's native typeface.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Finds the best matching font file and face index (for TTC) for a pattern via fontconfig.
fn fc_match(pattern: &str) -> Option<(PathBuf, u32)> {
    let out = Command::new("fc-match")
        .args(["--format=%{file}\n%{index}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let file = PathBuf::from(lines.next()?.trim());
    let index = lines.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
    is_usable(&file).then_some((file, index))
}

/// egui (ab_glyph) can only rasterize TrueType/OpenType outline fonts.
fn is_usable(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(ext.as_str(), "ttf" | "otf" | "ttc") && path.is_file()
}

/// Falls back to common distribution font paths if fontconfig is unavailable.
fn known_path(candidates: &[&str]) -> Option<(PathBuf, u32)> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| is_usable(p))
        .map(|p| (p, 0))
}

/// Last resort: first usable font file found inside standard font directories.
fn any_font_in(dirs: &[&str]) -> Option<(PathBuf, u32)> {
    fn walk(dir: &Path, depth: u8) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in &entries {
            if is_usable(p) {
                return Some(p.clone());
            }
        }
        entries
            .iter()
            .filter(|p| p.is_dir())
            .find_map(|p| walk(p, depth - 1))
    }
    dirs.iter()
        .find_map(|d| walk(Path::new(d), 4))
        .map(|p| (p, 0))
}

const SANS_PATHS: &[&str] = &[
    "/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf",
    "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/redhat/RedHatText-Regular.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

const MONO_PATHS: &[&str] = &[
    "/usr/share/fonts/hack-fonts/Hack-Regular.ttf",
    "/usr/share/fonts/hack/Hack-Regular.ttf",
    "/usr/share/fonts/TTF/Hack-Regular.ttf",
    "/usr/share/fonts/truetype/hack/Hack-Regular.ttf",
    "/usr/share/fonts/google-noto-vf/NotoSansMono[wght].ttf",
    "/usr/share/fonts/redhat/RedHatMono-Regular.ttf",
    "/usr/share/fonts/liberation-mono/LiberationMono-Regular.ttf",
    "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
];

/// Symbol fallback (arrows, triangles, box-drawing characters): Fedora ships Noto Sans Symbols / Math by default.
const SYMBOL_PATHS: &[&str] = &[
    "/usr/share/fonts/google-noto-vf/NotoSansSymbols[wght].ttf",
    "/usr/share/fonts/google-noto-symbols2/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSansSymbols2-Regular.ttf",
    "/usr/share/fonts/google-noto/NotoSansSymbols-Regular.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

const FONT_DIRS: &[&str] = &["/usr/share/fonts", "/usr/local/share/fonts"];

fn load_unique(
    name: &str,
    found: Option<(PathBuf, u32)>,
    loaded_files: &mut Vec<(PathBuf, u32, String)>,
    defs: &mut FontDefinitions,
) -> Option<String> {
    let (path, index) = found?;
    let existing = loaded_files.iter().find(|(p, i, _)| *p == path && *i == index);
    if let Some((_, _, existing_name)) = existing {
        return Some(existing_name.clone());
    }
    let bytes = std::fs::read(&path).ok()?;
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    defs.font_data.insert(name.to_owned(), data);
    loaded_files.push((path, index, name.to_owned()));
    Some(name.to_owned())
}

/// Builds egui font definitions from system fonts.
/// Returns `None` if no usable font is found (the caller prints a clear error message).
pub fn system_font_definitions() -> Option<FontDefinitions> {
    let mut defs = FontDefinitions::empty();
    let mut loaded_files = Vec::new();

    let sans = load_unique(
        "sans",
        fc_match("sans-serif")
            .or_else(|| known_path(SANS_PATHS))
            .or_else(|| any_font_in(FONT_DIRS)),
        &mut loaded_files,
        &mut defs,
    )?;
    let bold = load_unique(
        "sans_bold",
        fc_match("sans-serif:weight=bold"),
        &mut loaded_files,
        &mut defs,
    )
    .unwrap_or_else(|| sans.clone());
    let mono = load_unique(
        "mono",
        fc_match("monospace").or_else(|| known_path(MONO_PATHS)),
        &mut loaded_files,
        &mut defs,
    )
    .unwrap_or_else(|| sans.clone());
    let symbols = load_unique(
        "symbols",
        known_path(SYMBOL_PATHS).or_else(|| fc_match("Noto Sans Symbols 2")),
        &mut loaded_files,
        &mut defs,
    );

    let mut prop = vec![sans.clone()];
    let mut bold_fam = vec![bold, sans.clone()];
    let mut monos = vec![mono];
    if let Some(sym) = symbols {
        if sym != sans {
            prop.push(sym.clone());
            bold_fam.push(sym.clone());
            monos.push(sym);
        }
    }
    if !monos.contains(&sans) {
        monos.push(sans);
    }
    defs.families.insert(FontFamily::Proportional, prop);
    defs.families.insert(FontFamily::Name("bold".into()), bold_fam);
    defs.families.insert(FontFamily::Monospace, monos);
    Some(defs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_usable_rejects_bitmap_formats() {
        assert!(!is_usable(Path::new("/nonexistent/font.pcf.gz")));
        assert!(!is_usable(Path::new("/nonexistent/font.ttf")));
    }

    /// Verifies that egui can actually lay out text when system fonts are present
    /// (must not panic on missing or unsupported fonts).
    #[test]
    fn test_system_fonts_render_text() {
        let Some(defs) = system_font_definitions() else {
            eprintln!("No system fonts found; skipping test");
            return;
        };
        let ctx = eframe::egui::Context::default();
        ctx.set_fonts(defs);
        let _ = ctx.run(Default::default(), |ctx| {
            eframe::egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("Preview ‹ › × • ─ │");
                ui.monospace("fn main() {}");
            });
        });
    }
}
