mod app;
mod bridge;
mod fonts;
mod preview;

use app::{IpcCommand, QuickLookApp};
use eframe::egui;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("kquicklook.sock")
    } else {
        let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
        PathBuf::from(format!("/tmp/kquicklook-{user}.sock"))
    }
}

/// Sends a command to an already running kquicklook instance if one exists.
/// Returns `true` on success so the new process can exit immediately without consuming RAM.
fn try_send_to_existing_instance(target_path: Option<&Path>) -> bool {
    let sock = socket_path();
    if let Ok(mut stream) = UnixStream::connect(&sock) {
        let msg = match target_path {
            Some(p) => format!("OPEN:{}\n", p.display()),
            None => "TOGGLE\n".to_string(),
        };
        if stream.write_all(msg.as_bytes()).is_ok() {
            return true;
        }
    }
    // Remove stale socket file if connection failed
    let _ = std::fs::remove_file(&sock);
    false
}

/// Converts a CLI argument (plain path or `file://` URI) into a filesystem path.
fn parse_path_candidate(raw: &str) -> Option<PathBuf> {
    let first_line = raw.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }

    let cleaned = if let Some(stripped) = first_line.strip_prefix("file://") {
        url_decode(stripped)
    } else {
        first_line.to_string()
    };

    let path = PathBuf::from(cleaned);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn url_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.as_bytes().iter().copied();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                let hex = [h, l];
                if let Ok(s) = std::str::from_utf8(&hex) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
            }
            bytes.push(b'%');
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("KQuickLook - Lightweight Quick Preview Utility for KDE Plasma");
        println!("Usage:");
        println!("  kquicklook <file_path>    Preview file (updates if open; closes if same file)");
        println!("  kquicklook                Close open preview window");
        println!("  kquicklook --daemon       Dolphin Spacebar bridge (no GUI loaded)");
        return Ok(());
    }

    if args.iter().any(|a| a == "--daemon") {
        bridge::run_dolphin_space_bridge();
        return Ok(());
    }

    let requested_path: Option<PathBuf> = args
        .first()
        .map(|a| parse_path_candidate(a).unwrap_or_else(|| PathBuf::from(a)));

    // If an instance is already running, forward the signal and exit immediately
    if try_send_to_existing_instance(requested_path.as_deref()) {
        return Ok(());
    }

    let Some(initial_path) = requested_path else {
        eprintln!("KQuickLook: No file specified to preview. Usage: kquicklook <file>");
        return Ok(());
    };

    // Discover system fonts before opening the window (no fonts embedded in the binary)
    let Some(font_defs) = fonts::system_font_definitions() else {
        eprintln!("KQuickLook: No usable TTF/OTF font found on the system (fontconfig / fc-match).");
        std::process::exit(1);
    };

    // Create single-instance Unix socket
    let sock = socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).ok();

    let (tx, rx) = mpsc::channel::<IpcCommand>();

    let viewport = egui::ViewportBuilder::default()
        .with_title("KQuickLook")
        .with_app_id("kquicklook")
        .with_inner_size([860.0, 620.0])
        .with_min_inner_size([420.0, 300.0])
        .with_decorations(false) // Frameless modern window in macOS QuickLook style
        .with_transparent(true)
        .with_always_on_top()
        .with_active(true);

    let native_options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };

    let sock_cleanup = sock.clone();
    let result = eframe::run_native(
        "KQuickLook",
        native_options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(font_defs);
            // Lightweight background thread listening on the Unix socket
            if let Some(listener) = listener {
                let ctx = cc.egui_ctx.clone();
                thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let reader = BufReader::new(stream);
                        for line in reader.lines().map_while(Result::ok) {
                            if line == "TOGGLE" {
                                let _ = tx.send(IpcCommand::Toggle);
                                ctx.request_repaint();
                            } else if let Some(rest) = line.strip_prefix("OPEN:") {
                                let _ = tx.send(IpcCommand::Open(PathBuf::from(rest.trim())));
                                ctx.request_repaint();
                            }
                        }
                    }
                });
            }

            Ok(Box::new(QuickLookApp::new(cc.egui_ctx.clone(), initial_path, rx)))
        }),
    );

    let _ = std::fs::remove_file(&sock_cleanup);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_decode_utf8_and_spaces() {
        let encoded = "/home/user/Documents/Desktop%20File%20%C3%9C.pdf";
        assert_eq!(
            url_decode(encoded),
            "/home/user/Documents/Desktop File Ü.pdf"
        );
    }

    #[test]
    fn test_parse_path_candidate_with_file_uri() {
        let cargo_toml = format!("file://{}/Cargo.toml\n", env!("CARGO_MANIFEST_DIR"));
        let parsed = parse_path_candidate(&cargo_toml);
        assert!(parsed.is_some());
        assert!(parsed.unwrap().ends_with("Cargo.toml"));
    }

    #[test]
    fn test_parse_path_candidate_nonexistent() {
        assert!(parse_path_candidate("/nonexistent/path/123456789.xyz").is_none());
        assert!(parse_path_candidate("   ").is_none());
    }
}
