#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="$HOME/.config/llm-server-indicator"
AUTOSTART_DIR="$HOME/.config/autostart"
AUTOSTART_FILE="$AUTOSTART_DIR/llm-server-indicator.desktop"
SUDOERS_FILE="/etc/sudoers.d/llm-server-$USER"
TEMPLATE_UNIT_DEST="/etc/systemd/system/llm-server@.service"
LEGACY_UNIT="/etc/systemd/system/llm-server.service"
CTL_BIN_DEST="/usr/local/bin/llm-server-ctl"

echo "Uninstalling llm-server-indicator"

# Stop running template instances, remove the template unit, daemon-reload
if command -v systemctl >/dev/null 2>&1; then
  while IFS= read -r unit; do
    [[ -z "$unit" ]] && continue
    echo "Stopping $unit"
    sudo systemctl stop "$unit" || echo "Warning: failed to stop $unit" >&2
  done < <(systemctl list-units --state=running 'llm-server@*.service' --no-legend --plain 2>/dev/null | awk '{print $1}')
  if [[ -f "$TEMPLATE_UNIT_DEST" ]]; then
    sudo rm -f "$TEMPLATE_UNIT_DEST"
    echo "Removed template unit $TEMPLATE_UNIT_DEST"
  fi
  sudo systemctl daemon-reload || echo "Warning: daemon-reload failed (systemd unavailable?)" >&2
else
  echo "Warning: systemctl not found, skipping systemd unit cleanup" >&2
fi

# Remove installed binaries
if command -v cargo >/dev/null 2>&1; then
  if cargo uninstall llm-server-indicator 2>/dev/null; then
    echo "Removed llm-server-indicator via cargo uninstall"
  else
    echo "cargo uninstall failed, attempting manual removal"
    rm -f "$HOME/.cargo/bin/llm-server-indicator"
    rm -f "$HOME/.cargo/bin/llm-server-ctl"
  fi
fi
if [[ -f "$CTL_BIN_DEST" ]]; then
  sudo rm -f "$CTL_BIN_DEST"
  echo "Removed $CTL_BIN_DEST"
fi

# Remove sudoers file
if [[ -f "$SUDOERS_FILE" ]]; then
  sudo rm -f "$SUDOERS_FILE"
  echo "Removed sudoers file $SUDOERS_FILE"
fi

# Remove config directory
if [[ -d "$CONFIG_DIR" ]]; then
  rm -rf "$CONFIG_DIR"
  echo "Removed config directory $CONFIG_DIR"
fi

# Remove autostart entry (leftover from v1 installs)
if [[ -f "$AUTOSTART_FILE" ]]; then
  rm -f "$AUTOSTART_FILE"
  echo "Removed autostart entry $AUTOSTART_FILE"
fi

# Legacy v1 unit, if still present, is left untouched
if [[ -f "$LEGACY_UNIT" ]]; then
  echo "Note: legacy v1 unit $LEGACY_UNIT is still installed - leaving it untouched."
  echo "Run scripts/migrate_from_v1.sh to back it up and remove it."
fi

echo "Uninstall complete."
# Note: Waybar integration is left in place on purpose.
# Run scripts/install-waybar.sh manually if you want to update or remove it.
