#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

BIN_DIR="$HOME/.local/bin"
SERVICEMENU_DIR="$HOME/.local/share/kio/servicemenus"
APP_DIR="$HOME/.local/share/applications"
DOLPHIN_RC_DIR="$HOME/.local/share/kxmlgui5/dolphin"
DOLPHIN_RC="$DOLPHIN_RC_DIR/dolphinui.rc"
SYSTEMD_USER_DIR="$HOME/.config/systemd/user"

if [ "${1:-}" = "--uninstall" ] || [ "${1:-}" = "uninstall" ]; then
    echo "Uninstalling KQuickLook..."
    systemctl --user disable --now kquicklook-dolphin.service >/dev/null 2>&1 || true
    rm -f "$SYSTEMD_USER_DIR/kquicklook-dolphin.service" \
          "$BIN_DIR/kquicklook" \
          "$SERVICEMENU_DIR/kquicklook-service.desktop" \
          "$APP_DIR/kquicklook.desktop"
    if [ -f "$DOLPHIN_RC.kquicklook-backup" ]; then
        mv "$DOLPHIN_RC.kquicklook-backup" "$DOLPHIN_RC"
    else
        rm -f "$DOLPHIN_RC"
    fi
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    echo "KQuickLook removed. Restart Dolphin to restore default Space shortcut."
    exit 0
fi

echo "======================================================"
echo "  KQuickLook - KDE Plasma Installer Script (Rust)     "
echo "======================================================"

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: 'cargo' (Rust compiler) not found."
    if command -v dnf >/dev/null 2>&1; then
        echo "Install on Fedora KDE with:"
        echo "  sudo dnf install -y cargo rust poppler-utils ffmpeg-free wl-clipboard"
    fi
    exit 1
fi

if command -v dnf >/dev/null 2>&1; then
    missing_opt=()
    command -v pdftoppm >/dev/null 2>&1 || missing_opt+=("poppler-utils")
    command -v ffmpeg >/dev/null 2>&1   || missing_opt+=("ffmpeg-free")
    command -v wl-copy >/dev/null 2>&1  || missing_opt+=("wl-clipboard")
    if [ "${#missing_opt[@]}" -gt 0 ]; then
        echo "Note (Fedora): Optional preview helpers missing (${missing_opt[*]})."
        echo "  Run: sudo dnf install -y ${missing_opt[*]}"
    fi
fi

echo "[1/6] Building KQuickLook (release)..."
cargo build --release

mkdir -p "$BIN_DIR"
install -m 755 target/release/kquicklook "$BIN_DIR/kquicklook"
echo "[2/6] Binary installed: $BIN_DIR/kquicklook ($(du -h "$BIN_DIR/kquicklook" | cut -f1))"

# Dolphin context menu (Plasma 6) and desktop application entry
mkdir -p "$SERVICEMENU_DIR" "$APP_DIR"

sed "s|Exec=kquicklook %f|Exec=$BIN_DIR/kquicklook %f|g" kde/kquicklook-service.desktop > "$SERVICEMENU_DIR/kquicklook-service.desktop"
chmod +x "$SERVICEMENU_DIR/kquicklook-service.desktop"
sed "s|Exec=kquicklook %f|Exec=$BIN_DIR/kquicklook %f|g" kde/kquicklook.desktop > "$APP_DIR/kquicklook.desktop"
echo "[3/6] Dolphin service menu registered."

# --- Dolphin shortcuts ---
# Space: "Copy Location" instead of "Selection Mode" (the bridge service intercepts this and opens the preview).
#
# SAFETY NOTE: KXMLGUI uses whichever .rc file (local or system) has the HIGHEST version number.
# When the local file has a lower version, only its <ActionProperties> (shortcuts) are merged
# into Dolphin's own UI definition. Therefore we write shortcut-only files with version="1"
# so Dolphin's menus and toolbars are NEVER wiped out.
mkdir -p "$DOLPHIN_RC_DIR"

if [ -f "$DOLPHIN_RC" ] && [ ! -f "$DOLPHIN_RC.kquicklook-backup" ]; then
    cp "$DOLPHIN_RC" "$DOLPHIN_RC.kquicklook-backup"
    echo "      Backed up existing Dolphin shortcut config: $DOLPHIN_RC.kquicklook-backup"
fi

python3 - "$DOLPHIN_RC" << 'PYEOF'
import os, sys
import xml.etree.ElementTree as ET

rc_path = sys.argv[1]
root = None
if os.path.exists(rc_path):
    try:
        root = ET.parse(rc_path).getroot()
    except ET.ParseError:
        root = None

if root is None:
    root = ET.Element("gui", {"name": "dolphin", "version": "1"})

# Files that only carry shortcut overrides (no MenuBar/ToolBar) must always be
# marked with a low version so they never override Dolphin's default UI layout.
if root.find("MenuBar") is None and root.find("ToolBar") is None:
    root.set("version", "1")

ap = root.find("ActionProperties")
if ap is None:
    ap = ET.SubElement(root, "ActionProperties", {"scheme": "Default"})

def set_shortcut(name, sc):
    for act in ap.findall("Action"):
        if act.get("name") == name:
            act.set("shortcut", sc)
            return
    ET.SubElement(ap, "Action", {"name": name, "shortcut": sc})

set_shortcut("toggle_selection_mode", "")
set_shortcut("copy_location", "Space; Ctrl+Alt+C")

body = ET.tostring(root, encoding="unicode")
with open(rc_path, "w", encoding="utf-8") as f:
    f.write('<!DOCTYPE gui SYSTEM "kpartgui.dtd">\n')
    f.write(body)
    f.write("\n")
PYEOF
echo "[4/6] Dolphin shortcut configured (Space -> KQuickLook)."

# --- KWin window rule ---
# Under Wayland, KWin may refuse focus to windows spawned by background processes
# (focus stealing prevention). This rule forces focus and "keep above" specifically for KQuickLook.
if command -v kwriteconfig6 >/dev/null 2>&1 && command -v kreadconfig6 >/dev/null 2>&1; then
    RULE_ID="kquicklook-focus"
    rules="$(kreadconfig6 --file kwinrulesrc --group General --key rules)"
    case ",$rules," in
        *",$RULE_ID,"*) ;;
        *)
            if [ -z "$rules" ]; then rules="$RULE_ID"; else rules="$rules,$RULE_ID"; fi
            kwriteconfig6 --file kwinrulesrc --group General --key rules "$rules"
            kwriteconfig6 --file kwinrulesrc --group General --key count \
                "$(printf '%s' "$rules" | tr ',' '\n' | grep -c .)"
            ;;
    esac
    kw() { kwriteconfig6 --file kwinrulesrc --group "$RULE_ID" --key "$1" "$2"; }
    kw Description "KQuickLook preview window"
    kw wmclass "kquicklook";  kw wmclassmatch 1        # exact match
    kw fsplevel 0;            kw fsplevelrule 2        # focus stealing prevention: none (force)
    kw acceptfocus true;      kw acceptfocusrule 2
    kw above true;            kw aboverule 2           # keep above
    kw skiptaskbar true;      kw skiptaskbarrule 2     # skip taskbar
    busctl --user call org.kde.KWin /KWin org.kde.KWin reconfigure >/dev/null 2>&1 || true
    echo "[5/6] KWin window rule added (focus + keep above)."
else
    echo "[5/6] kwriteconfig6 not found, skipping KWin rule."
fi

# --- Bridge service (systemd --user) ---
# Bound to graphical-session.target: starts after the Plasma session and WAYLAND_DISPLAY/DISPLAY
# are ready, and stops when the session ends. Placed in background.slice for Fedora uresourced.
mkdir -p "$SYSTEMD_USER_DIR"

cat << EOF > "$SYSTEMD_USER_DIR/kquicklook-dolphin.service"
[Unit]
Description=KQuickLook Dolphin Spacebar Bridge
PartOf=graphical-session.target
After=graphical-session.target plasma-workspace.target

[Service]
Type=simple
Slice=background.slice
MemoryHigh=32M
ExecStart=$BIN_DIR/kquicklook --daemon
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
EOF

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload
    # Clean up legacy default.target symlink if present
    systemctl --user disable kquicklook-dolphin.service >/dev/null 2>&1 || true
    rm -f "$SYSTEMD_USER_DIR/default.target.wants/kquicklook-dolphin.service"
    systemctl --user enable kquicklook-dolphin.service
    systemctl --user restart kquicklook-dolphin.service
fi

echo "[6/6] Installation complete!"
echo ""
echo " Close and reopen any running Dolphin windows."
echo " Select a file in Dolphin and press Space to open KQuickLook."
