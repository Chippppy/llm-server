#!/usr/bin/env bash
set -euo pipefail

# Waybar integration installer for llm-server-indicator (v2).
# Installs the repo module file over the module file the Waybar config
# imports (~/.config/waybar/modules/llm-server-module.json) and appends the
# project style.css. The Waybar config itself is not modified: it already
# imports the module file and lists "custom/llm-server" in modules-left.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WAYBAR_DIR="$HOME/.config/waybar"
WAYBAR_CONFIG="$WAYBAR_DIR/config.jsonc"
WAYBAR_STYLE="$WAYBAR_DIR/style.css"
MODULE_SRC="$SCRIPT_DIR/waybar/llm-server-module.json"
MODULE_DEST="$WAYBAR_DIR/modules/llm-server-module.json"
GROUP_SRC="$SCRIPT_DIR/waybar/group-llm.json"
GROUP_DEST="$WAYBAR_DIR/groups/group-llm.json"

DRY_RUN=${DRY_RUN:-0}

if [[ ! -f "$MODULE_SRC" ]]; then
  echo "Error: module file not found at $MODULE_SRC" >&2
  exit 1
fi

if [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Waybar integration would be installed"
  echo "[DRY-RUN] Would backup existing $MODULE_DEST (if present) to ${MODULE_DEST}.bak"
  echo "[DRY-RUN] Would copy $MODULE_SRC to $MODULE_DEST"
  echo "[DRY-RUN] Would backup existing $GROUP_DEST (if present) to ${GROUP_DEST}.bak"
  echo "[DRY-RUN] Would copy $GROUP_SRC to $GROUP_DEST"
  echo "[DRY-RUN] Would patch $WAYBAR_CONFIG (group include + modules entry, if needed)"
  exit 0
fi

echo "Installing Waybar integration for llm-server-indicator"
echo "Project dir: $SCRIPT_DIR"

mkdir -p "$(dirname "$MODULE_DEST")"

# Back up the live module file before overwriting it
if [[ -f "$MODULE_DEST" ]]; then
  cp "$MODULE_DEST" "${MODULE_DEST}.bak"
  echo "Backed up existing module file to ${MODULE_DEST}.bak"
fi

cp "$MODULE_SRC" "$MODULE_DEST"
echo "Installed module file $MODULE_DEST"

# Install the drawer group (leader + per-model buttons that slide out)
if [[ -f "$GROUP_SRC" ]]; then
  mkdir -p "$(dirname "$GROUP_DEST")"
  if [[ -f "$GROUP_DEST" ]]; then
    cp "$GROUP_DEST" "${GROUP_DEST}.bak"
    echo "Backed up existing group file to ${GROUP_DEST}.bak"
  fi
  cp "$GROUP_SRC" "$GROUP_DEST"
  echo "Installed group file $GROUP_DEST"
else
  echo "Warning: group file not found at $GROUP_SRC"
fi

# Wire the group into the Waybar config (idempotent, with backup):
#  1. include the group file next to the module file
#  2. list "group/llm" in the modules array (replacing "custom/llm-server")
if [[ ! -f "$WAYBAR_CONFIG" ]]; then
  echo "Warning: $WAYBAR_CONFIG not found."
  echo "Add to your Waybar config manually:"
  echo '  include: "~/.config/waybar/groups/group-llm.json"'
  echo '  modules: "group/llm" (in place of "custom/llm-server")'
else
  if ! grep -q "group-llm.json" "$WAYBAR_CONFIG"; then
    if grep -q "modules/llm-server-module.json" "$WAYBAR_CONFIG"; then
      sed -i '/modules\/llm-server-module\.json/a\        "~/.config/waybar/groups/group-llm.json",' "$WAYBAR_CONFIG"
      echo "Added group include to $WAYBAR_CONFIG"
    else
      echo "Warning: $WAYBAR_CONFIG does not import $MODULE_DEST."
      echo 'Add manually: "~/.config/waybar/groups/group-llm.json" to the "include" array.'
    fi
  fi
  if grep -q '"custom/llm-server"' "$WAYBAR_CONFIG"; then
    cp "$WAYBAR_CONFIG" "${WAYBAR_CONFIG}.bak"
    sed -i 's#"custom/llm-server"#"group/llm"#' "$WAYBAR_CONFIG"
    echo "Replaced \"custom/llm-server\" with \"group/llm\" in $WAYBAR_CONFIG (backup: ${WAYBAR_CONFIG}.bak)"
  elif ! grep -q '"group/llm"' "$WAYBAR_CONFIG"; then
    echo "Warning: neither \"custom/llm-server\" nor \"group/llm\" found in $WAYBAR_CONFIG."
    echo 'Add "group/llm" to modules-left or modules-right.'
  fi
fi

# Install project style.css into the live style.css. Idempotent: any
# previously appended llm block (marker to end-of-file) is removed first,
# so re-running the installer never accumulates duplicate rules.
PROJECT_STYLE="$SCRIPT_DIR/waybar/style.css"
LLM_STYLE_MARKER="/* llm-server-indicator Waybar styles - appended by install script */"
if [[ -f "$PROJECT_STYLE" ]]; then
  # Backup existing style.css before modifying
  if [[ -e "$WAYBAR_STYLE" && ! -L "$WAYBAR_STYLE" ]]; then
    cp "$WAYBAR_STYLE" "${WAYBAR_STYLE}.bak.$(date +%s)"
    echo "Backed up existing style.css"
  fi
  # Remove a previously appended llm block, if present
  if [[ -f "$WAYBAR_STYLE" ]]; then
    marker_line=$(grep -nF "$LLM_STYLE_MARKER" "$WAYBAR_STYLE" | head -1 | cut -d: -f1 || true)
    if [[ -n "${marker_line:-}" ]]; then
      sed -i "${marker_line},\$d" "$WAYBAR_STYLE"
      echo "Removed previously appended llm styles (line ${marker_line} onward)"
    fi
  fi
  # Append the current style with a marker comment
  {
    echo ""
    echo "$LLM_STYLE_MARKER"
    cat "$PROJECT_STYLE"
  } >> "$WAYBAR_STYLE"
  echo "Appended llm-server-indicator styles to $WAYBAR_STYLE"
else
  echo "Warning: project style.css not found at $PROJECT_STYLE"
fi

echo "Waybar integration installed."
echo "Drawer group: hover the LLM status to slide out the model buttons."
echo "Buttons: left-click = toggle model, right-click = stop model. Leader: left-click = GUI."
echo "Restart Waybar to apply changes."
