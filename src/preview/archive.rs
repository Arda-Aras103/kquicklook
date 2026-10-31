use crate::preview::{format_bytes, PreviewContent};
use flate2::read::GzDecoder;
use std::fs::File;
use std::path::Path;
use std::process::Command;

const MAX_LIST_ENTRIES: usize = 500;

pub fn load_directory(path: &Path) -> PreviewContent {
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return PreviewContent::Unsupported {
            reason: "Failed to read folder contents.".to_string(),
            hex_preview: String::new(),
        };
    };

    let mut entries = Vec::new();
    let mut dir_count = 0usize;
    let mut file_count = 0usize;
    let mut total_bytes = 0u64;

    for entry in read_dir.flatten() {
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        if is_dir {
            dir_count += 1;
        } else {
            file_count += 1;
            total_bytes += size;
        }

        if entries.len() < MAX_LIST_ENTRIES {
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = if is_dir { "/" } else { "" };
            let size_label = if is_dir {
                "Folder".to_string()
            } else {
                format_bytes(size)
            };
            entries.push((format!("{name}{suffix}"), size_label));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let summary = format!(
        "Folder Summary: {} Subfolders, {} Files (Total: {})",
        dir_count,
        file_count,
        format_bytes(total_bytes)
    );

    PreviewContent::DirectoryOrArchive { summary, entries }
}

pub fn load_zip_archive(path: &Path) -> PreviewContent {
    let Ok(file) = File::open(path) else {
        return PreviewContent::Unsupported {
            reason: "Failed to open ZIP archive.".to_string(),
            hex_preview: String::new(),
        };
    };

    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        return PreviewContent::Unsupported {
            reason: "Invalid or corrupted ZIP archive.".to_string(),
            hex_preview: String::new(),
        };
    };

    let total_count = archive.len();
    let mut total_uncompressed = 0u64;
    let mut entries = Vec::new();

    for i in 0..total_count {
        if let Ok(entry) = archive.by_index(i) {
            let size = entry.size();
            total_uncompressed += size;
            if entries.len() < MAX_LIST_ENTRIES {
                entries.push((
                    entry.name().to_string(),
                    format_bytes(size),
                ));
            }
        }
    }

    let summary = format!(
        "ZIP Archive: {} items (Uncompressed Size: {})",
        total_count,
        format_bytes(total_uncompressed)
    );

    PreviewContent::DirectoryOrArchive { summary, entries }
}

pub fn load_tar_archive(path: &Path) -> PreviewContent {
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_other_compressed = file_name.ends_with(".tar.xz")
        || file_name.ends_with(".txz")
        || file_name.ends_with(".tar.zst")
        || file_name.ends_with(".tzst")
        || file_name.ends_with(".tar.bz2")
        || file_name.ends_with(".tbz2");

    if is_other_compressed {
        return load_system_tar_archive(path);
    }

    let Ok(file) = File::open(path) else {
        return PreviewContent::Unsupported {
            reason: "Failed to open TAR archive.".to_string(),
            hex_preview: String::new(),
        };
    };

    let is_gzip = file_name.ends_with(".tar.gz")
        || file_name.ends_with(".tgz")
        || file_name.ends_with(".gz");

    let mut entries = Vec::new();
    let mut total_count = 0usize;
    let mut total_size = 0u64;

    if is_gzip {
        let gz = GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        if let Ok(iter) = archive.entries() {
            for entry in iter.flatten() {
                total_count += 1;
                let size = entry.size();
                total_size += size;
                if entries.len() < MAX_LIST_ENTRIES {
                    if let Ok(p) = entry.path() {
                        entries.push((p.display().to_string(), format_bytes(size)));
                    }
                }
            }
        }
    } else {
        let mut archive = tar::Archive::new(file);
        if let Ok(iter) = archive.entries() {
            for entry in iter.flatten() {
                total_count += 1;
                let size = entry.size();
                total_size += size;
                if entries.len() < MAX_LIST_ENTRIES {
                    if let Ok(p) = entry.path() {
                        entries.push((p.display().to_string(), format_bytes(size)));
                    }
                }
            }
        }
    }

    let summary = format!(
        "TAR Archive: {} items (Total Size: {})",
        total_count,
        format_bytes(total_size)
    );

    PreviewContent::DirectoryOrArchive { summary, entries }
}

fn load_system_tar_archive(path: &Path) -> PreviewContent {
    let Ok(out) = Command::new("tar").arg("-tvf").arg(path).output() else {
        return PreviewContent::Unsupported {
            reason: "Failed to run 'tar' to inspect compressed archive.".to_string(),
            hex_preview: String::new(),
        };
    };
    if !out.status.success() {
        return PreviewContent::Unsupported {
            reason: "Invalid or unsupported compressed TAR archive.".to_string(),
            hex_preview: String::new(),
        };
    }

    let mut entries = Vec::new();
    let mut total_count = 0usize;
    let mut total_size = 0u64;

    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // Standard `tar -tvf` line: `drwxr-xr-x owner/group 0 2026-01-01 00:00 path/to/entry`
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 6 {
            continue;
        }
        let name = tokens[5..].join(" ");
        let size = tokens[2].parse::<u64>().unwrap_or(0);
        total_count += 1;
        total_size += size;
        if entries.len() < MAX_LIST_ENTRIES {
            let label = if name.ends_with('/') {
                "Folder".to_string()
            } else {
                format_bytes(size)
            };
            entries.push((name, label));
        }
    }

    let summary = format!(
        "Compressed TAR Archive: {} items (Total Size: {})",
        total_count,
        format_bytes(total_size)
    );
    PreviewContent::DirectoryOrArchive { summary, entries }
}

/// Inspects a Fedora / Red Hat `.rpm` package via `rpm -qip` and `rpm -qlvp`.
pub fn load_rpm_package(path: &Path) -> PreviewContent {
    let Ok(info_out) = Command::new("rpm").arg("-qip").arg(path).output() else {
        return PreviewContent::Unsupported {
            reason: "'rpm' utility not found to inspect RPM package.".to_string(),
            hex_preview: String::new(),
        };
    };
    if !info_out.status.success() {
        return PreviewContent::Unsupported {
            reason: "Invalid or corrupted RPM package.".to_string(),
            hex_preview: String::new(),
        };
    }

    let mut entries = Vec::new();
    let info_text = String::from_utf8_lossy(&info_out.stdout);
    let mut pkg_title = String::from("RPM Package");

    for line in info_text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            let is_meta_key = matches!(
                key,
                "Name" | "Version" | "Release" | "Architecture" | "Summary" | "License" | "Size"
            );
            if is_meta_key && !val.is_empty() {
                if key == "Name" {
                    pkg_title = format!("RPM Package: {val}");
                }
                entries.push((format!("[Meta] {key}"), val.to_string()));
            }
        }
    }

    let mut file_count = 0usize;
    if let Ok(list_out) = Command::new("rpm")
        .args(["-q", "--qf", "[%{FILENAMES}\t%{FILESIZES}\n]", "-p"])
        .arg(path)
        .output()
    {
        if list_out.status.success() {
            for line in String::from_utf8_lossy(&list_out.stdout).lines() {
                if line.trim().is_empty() || line.contains("(contains no files)") {
                    continue;
                }
                file_count += 1;
                if entries.len() < MAX_LIST_ENTRIES {
                    if let Some((fname, fsize)) = line.split_once('\t') {
                        let sz = fsize.trim().parse::<u64>().unwrap_or(0);
                        entries.push((fname.to_string(), format_bytes(sz)));
                    } else {
                        entries.push((line.to_string(), String::new()));
                    }
                }
            }
        }
    }

    let summary = format!("{pkg_title} ({file_count} packaged files)");
    PreviewContent::DirectoryOrArchive { summary, entries }
}
