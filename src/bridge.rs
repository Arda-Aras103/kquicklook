//! KDE session D-Bus helpers and Dolphin Spacebar bridge.
//!
//! All D-Bus calls are executed via `busctl`, which ships with systemd on every distribution
//! (avoiding dependencies on distro-specific binaries like `qdbus` / `qdbus6` / `qdbus-qt6`).

use std::env;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs `busctl --user --json=short <args>` and returns the parsed JSON output.
fn busctl_json(args: &[&str]) -> Option<serde_json::Value> {
    let out = Command::new("busctl")
        .arg("--user")
        .arg("--json=short")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Extracts the first item from a D-Bus response (`{"type":..,"data":[x]}` -> x).
fn first_data(v: &serde_json::Value) -> Option<&serde_json::Value> {
    v.get("data")?.as_array()?.first()
}

fn klipper_get() -> Option<String> {
    let v = busctl_json(&[
        "call",
        "org.kde.klipper",
        "/klipper",
        "org.kde.klipper.klipper",
        "getClipboardContents",
    ])?;
    first_data(&v)?.as_str().map(str::to_owned)
}

fn klipper_set(text: &str) {
    let _ = Command::new("busctl")
        .args([
            "--user",
            "call",
            "org.kde.klipper",
            "/klipper",
            "org.kde.klipper.klipper",
            "setClipboardContents",
            "s",
            text,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn dolphin_services() -> Vec<String> {
    let Some(v) = busctl_json(&[
        "call",
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ListNames",
    ]) else {
        return Vec::new();
    };
    first_data(&v)
        .and_then(|d| d.as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|n| n.as_str())
                .filter(|n| *n == "org.kde.dolphin" || n.starts_with("org.kde.dolphin-"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Converts a filesystem path into a `file://` URL (RFC 3986 percent-encoding).
pub fn path_to_file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

#[derive(Debug, PartialEq)]
enum Origin {
    /// Path is visible in an open Dolphin view -> triggered via Space.
    Dolphin,
    /// Dolphin is open, or not showing this item -> copied from another application.
    Elsewhere,
    /// Dolphin does not support this check (older version) -> accept.
    Unknown,
}

/// Verifies whether the clipboard path came from Dolphin using Dolphin's own
/// `isItemVisibleInAnyView` D-Bus method.
fn origin_of(path: &Path) -> Origin {
    let services = dolphin_services();
    if services.is_empty() {
        // "Copy Location" can only originate from Dolphin; if Dolphin isn't running, it came from elsewhere.
        return Origin::Elsewhere;
    }
    let url = path_to_file_url(path);
    let mut any_answer = false;
    for service in &services {
        for i in 1..=8 {
            let obj = format!("/dolphin/Dolphin_{i}");
            let Some(v) = busctl_json(&[
                "call",
                service,
                &obj,
                "org.kde.dolphin.MainWindow",
                "isItemVisibleInAnyView",
                "s",
                &url,
            ]) else {
                break; // Window does not exist or method is unsupported
            };
            any_answer = true;
            if first_data(&v).and_then(|b| b.as_bool()) == Some(true) {
                return Origin::Dolphin;
            }
        }
    }
    if any_answer {
        Origin::Elsewhere
    } else {
        Origin::Unknown
    }
}

/// Output of "Copy Location": single-line existing local path starting with `/`.
pub fn as_copied_location(content: &str) -> Option<PathBuf> {
    let t = content.trim_end_matches(['\n', '\r']);
    if !t.starts_with('/') || t.contains('\n') {
        return None;
    }
    let p = PathBuf::from(t);
    p.exists().then_some(p)
}

/// Restores the previous clipboard contents. If it was a file copy (`file://` URI list),
/// writes it back with the proper MIME type so files copied via Ctrl+C remain pasteable.
fn restore_clipboard(prev: &str) {
    let is_uri_list = !prev.is_empty() && prev.lines().all(|l| l.starts_with("file://"));
    if is_uri_list {
        let wayland = env::var_os("WAYLAND_DISPLAY").is_some();
        let (cmd, args): (&str, &[&str]) = if wayland {
            ("wl-copy", &["--type", "text/uri-list"])
        } else {
            ("xclip", &["-selection", "clipboard", "-t", "text/uri-list"])
        };
        if let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(prev.replace('\n', "\r\n").as_bytes());
            } // stdin closes here
            // wl-copy/xclip forks into the background and the foreground process exits right away;
            // reap it so no zombie process is left behind.
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return;
            }
        }
    }
    klipper_set(prev);
}

fn spawn_monitor() -> Option<std::process::Child> {
    const MATCH: &str =
        "type='signal',interface='org.kde.klipper.klipper',member='clipboardHistoryUpdated'";
    // Prefer `busctl --json=short monitor` (ships with systemd on Fedora / all modern distros
    // and flushes each signal line immediately), falling back to `dbus-monitor`.
    Command::new("busctl")
        .args([
            "--user",
            "--json=short",
            "monitor",
            &format!("--match={MATCH}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .or_else(|_| {
            Command::new("dbus-monitor")
                .args(["--session", MATCH])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        })
        .ok()
}

/// Listens only to Klipper D-Bus signals without initializing a GUI.
/// When Space ("Copy Location") is pressed in Dolphin:
///  1. Verifies that the path came from an open Dolphin view,
///  2. Spawns the preview window in a separate process,
///  3. Restores the previous clipboard contents (keeping the user's clipboard intact).
pub fn run_dolphin_space_bridge() {
    let exe_path = env::current_exe().unwrap_or_else(|_| PathBuf::from("kquicklook"));

    let Some(mut child) = spawn_monitor() else {
        eprintln!("KQuickLook: Failed to start dbus-monitor/busctl.");
        std::process::exit(1);
    };
    let Some(stdout) = child.stdout.take() else {
        return;
    };

    // The user's last "real" clipboard item (excluding paths triggered via Space)
    let mut last_user_content: Option<String> = klipper_get();
    // Used to ignore the signal triggered by our own clipboard restoration
    let mut ignore_content: Option<String> = None;
    let mut last_trigger: Option<(PathBuf, Instant)> = None;

    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if !line.contains("clipboardHistoryUpdated") {
            continue;
        }
        let Some(content) = klipper_get() else {
            continue;
        };

        if ignore_content.as_deref() == Some(content.as_str()) {
            ignore_content = None;
            continue;
        }

        let Some(path) = as_copied_location(&content) else {
            last_user_content = Some(content);
            continue;
        };

        // Klipper may emit multiple signals for the same clipboard change
        if let Some((p, t)) = &last_trigger {
            if *p == path && t.elapsed() < Duration::from_millis(400) {
                continue;
            }
        }

        if origin_of(&path) == Origin::Elsewhere {
            // User copied a plain-text path from another app: leave it untouched
            last_user_content = Some(content);
            continue;
        }

        last_trigger = Some((path.clone(), Instant::now()));
        let _ = Command::new(&exe_path).arg(&path).spawn();

        if let Some(prev) = &last_user_content {
            if *prev != content {
                ignore_content = Some(prev.clone());
                restore_clipboard(prev);
            }
        }
    }

    let _ = child.kill();
    // Exit with non-zero status so systemd (Restart=on-failure) restarts if dbus-monitor dies
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_to_file_url() {
        assert_eq!(
            path_to_file_url(Path::new("/home/a/Desktop/my file.txt")),
            "file:///home/a/Desktop/my%20file.txt"
        );
    }

    #[test]
    fn test_as_copied_location() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        assert!(as_copied_location(&format!("{manifest}/Cargo.toml")).is_some());
        assert!(as_copied_location(&format!("{manifest}/Cargo.toml\n")).is_some());
        assert!(as_copied_location(&format!("file://{manifest}/Cargo.toml")).is_none());
        assert!(as_copied_location("/a\n/b").is_none());
        assert!(as_copied_location("hello world").is_none());
        assert!(as_copied_location("/nonexistent/path/123.xyz").is_none());
    }
}
