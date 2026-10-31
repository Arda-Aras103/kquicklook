use crate::preview::PreviewContent;
use std::path::Path;
use std::process::Command;

pub fn load_pdf_preview(path: &Path, page: usize) -> PreviewContent {
    let total_pages = query_pdf_page_count(path).unwrap_or(0);
    let safe_page = if total_pages > 0 {
        page.clamp(1, total_pages)
    } else {
        page.max(1)
    };

    // 1. If pdftoppm (poppler-utils) is installed, render the page directly into memory as PNG
    if let Some((rgba, w, h)) = render_page_via_pdftoppm(path, safe_page) {
        return PreviewContent::Pdf {
            page_image: Some((rgba, w, h)),
            current_page: safe_page,
            total_pages,
            info_lines: Vec::new(),
        };
    }

    // 2. Fall back to text extraction via pdftotext + pdfinfo when pdftoppm is unavailable
    let mut info_lines = Vec::new();
    if let Ok(out) = Command::new("pdfinfo").arg(path).output() {
        if out.status.success() {
            info_lines.push("─── PDF Document Info ───".to_string());
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                info_lines.push(line.to_string());
            }
            info_lines.push(String::new());
        }
    }

    if let Ok(out) = Command::new("pdftotext")
        .args([
            "-f",
            &safe_page.to_string(),
            "-l",
            &safe_page.to_string(),
            path.to_str().unwrap_or(""),
            "-",
        ])
        .output()
    {
        if out.status.success() {
            info_lines.push(format!("─── Page {safe_page} Text Content ───"));
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                info_lines.push(line.to_string());
            }
        }
    }

    if info_lines.is_empty() {
        info_lines.push(
            "Install 'poppler-utils' (pdftoppm) for full visual PDF page rendering."
                .to_string(),
        );
    }

    PreviewContent::Pdf {
        page_image: None,
        current_page: safe_page,
        total_pages,
        info_lines,
    }
}

fn query_pdf_page_count(path: &Path) -> Option<usize> {
    let out = Command::new("pdfinfo").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

fn render_page_via_pdftoppm(path: &Path, page: usize) -> Option<(Vec<u8>, usize, usize)> {
    let page_str = page.to_string();
    let output = Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            "125",
            "-f",
            &page_str,
            "-l",
            &page_str,
            "-singlefile",
        ])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    let img = image::load_from_memory(&output.stdout).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w as usize, h as usize))
}
