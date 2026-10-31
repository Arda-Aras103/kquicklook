pub mod archive;
pub mod code;
pub mod image;
pub mod media;
pub mod pdf;

use eframe::egui::Color32;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct FileMetadata {
    pub file_name: String,
    pub human_size: String,
    pub kind_label: String,
    pub modified_str: String,
    pub icon: String,
}

impl FileMetadata {
    pub fn from_path(path: &Path) -> Self {
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string();

        let meta = std::fs::metadata(path).ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        let human_size = if is_dir {
            "Folder".to_string()
        } else {
            format_bytes(bytes)
        };

        let modified_str = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(format_system_time)
            .unwrap_or_else(|| "-".to_string());

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let kind_label = if is_dir {
            "Folder".to_string()
        } else {
            match ext.as_str() {
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico" | "tiff" | "svg" => {
                    format!("{} Image", ext.to_uppercase())
                }
                "pdf" => "PDF Document".to_string(),
                "mp4" | "mkv" | "webm" | "avi" | "mov" => {
                    format!("{} Video", ext.to_uppercase())
                }
                "mp3" | "flac" | "ogg" | "wav" | "m4a" | "opus" => {
                    format!("{} Audio", ext.to_uppercase())
                }
                "rpm" => "RPM Package".to_string(),
                "zip" | "tar" | "gz" | "tgz" | "xz" | "txz" | "zst" | "tzst" | "bz2" | "tbz2" => {
                    "Archive File".to_string()
                }
                "rs" | "py" | "js" | "ts" | "c" | "cpp" | "h" | "go" | "sh" | "json" | "toml"
                | "yaml" | "yml" | "md" | "html" | "css" | "xml" | "sql" => {
                    format!("{} Source / Text", ext.to_uppercase())
                }
                _ => "File".to_string(),
            }
        };

        Self {
            file_name,
            human_size,
            kind_label,
            modified_str,
            icon: if is_dir {
                "FOLDER".to_string()
            } else if ext.is_empty() {
                "FILE".to_string()
            } else {
                ext.to_uppercase()
            },
        }
    }
}

pub enum PreviewContent {
    Image {
        rgba: Vec<u8>,
        width: usize,
        height: usize,
        original_dimensions: (u32, u32),
    },
    /// Animated GIF: list of (RGBA frame, delay in ms)
    Animation {
        frames: Vec<(Vec<u8>, u32)>,
        width: usize,
        height: usize,
        original_dimensions: (u32, u32),
    },
    Code {
        highlighted_sections: Vec<(Color32, Color32, String)>,
        raw_text: String,
        syntax_name: String,
        truncated: bool,
        is_markdown: bool,
    },
    Pdf {
        page_image: Option<(Vec<u8>, usize, usize)>,
        current_page: usize,
        total_pages: usize,
        info_lines: Vec<String>,
    },
    Media {
        kind_title: String,
        is_video: bool,
        duration_secs: f64,
        video_dims: Option<(u32, u32)>,
        thumbnail: Option<(Vec<u8>, usize, usize)>,
        metadata_pairs: Vec<(String, String)>,
    },
    DirectoryOrArchive {
        summary: String,
        entries: Vec<(String, String)>,
    },
    Unsupported {
        reason: String,
        hex_preview: String,
    },
}

pub struct PreviewLoader;

impl PreviewLoader {
    pub fn load(path: &Path, pdf_page: usize) -> PreviewContent {
        if !path.exists() {
            return PreviewContent::Unsupported {
                reason: "File not found or permission denied.".to_string(),
                hex_preview: String::new(),
            };
        }

        if path.is_dir() {
            return archive::load_directory(path);
        }

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match ext.as_str() {
            "gif" => image::load_gif(path),
            "png" | "jpg" | "jpeg" | "webp" | "bmp" | "ico" | "tiff" | "tif" => {
                image::load_raster_image(path)
            }
            "svg" => image::load_svg_image(path),
            "pdf" => pdf::load_pdf_preview(path, pdf_page),
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "m4v" => {
                media::load_media_preview(path, true)
            }
            "mp3" | "flac" | "ogg" | "wav" | "m4a" | "opus" | "aac" => {
                media::load_media_preview(path, false)
            }
            "rpm" => archive::load_rpm_package(path),
            "zip" => archive::load_zip_archive(path),
            "tar" | "gz" | "tgz" | "xz" | "txz" | "zst" | "tzst" | "bz2" | "tbz2" => {
                archive::load_tar_archive(path)
            }
            _ => {
                // Detect whether the file is text/code by checking for NUL bytes in the first 4 KB
                if is_likely_text_file(path) {
                    code::load_code_or_text(path, &ext)
                } else {
                    PreviewContent::Unsupported {
                        reason: "No direct visual preview available for this binary file format."
                            .to_string(),
                        hex_preview: read_hex_dump(path, 512),
                    }
                }
            }
        }
    }
}

fn is_likely_text_file(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    if n == 0 {
        return true;
    }
    // Treat as text file if there is no '\0' (null byte) within the first 4 KB
    !buf[..n].contains(&0)
}

fn read_hex_dump(path: &Path, max_bytes: usize) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let mut buf = vec![0u8; max_bytes];
    let Ok(n) = f.read(&mut buf) else {
        return String::new();
    };
    buf.truncate(n);

    let mut out = String::new();
    for (idx, chunk) in buf.chunks(16).enumerate() {
        out.push_str(&format!("{:04x}  ", idx * 16));
        for i in 0..16 {
            if let Some(b) = chunk.get(i) {
                out.push_str(&format!("{b:02x} "));
            } else {
                out.push_str("   ");
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for &b in chunk {
            let c = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            };
            out.push(c);
        }
        out.push_str("|\n");
    }
    out
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn format_system_time(t: SystemTime) -> String {
    let Ok(dur) = t.duration_since(UNIX_EPOCH) else {
        return "-".to_string();
    };
    let secs = dur.as_secs();
    let days = secs / 86_400;
    // Simple Gregorian date calculation without external date library dependencies
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn test_code_preview_loads_cargo_toml() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let content = PreviewLoader::load(&path, 1);
        match content {
            PreviewContent::Code {
                raw_text,
                truncated,
                is_markdown,
                ..
            } => {
                assert!(raw_text.contains("kquicklook"));
                assert!(!truncated);
                assert!(!is_markdown);
            }
            _ => panic!("Cargo.toml should have been loaded as text/code"),
        }
    }

    #[test]
    fn test_directory_preview() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let content = PreviewLoader::load(&path, 1);
        match content {
            PreviewContent::DirectoryOrArchive { entries, .. } => {
                assert!(!entries.is_empty());
            }
            _ => panic!("src/ directory should have been loaded as a directory preview"),
        }
    }
}
