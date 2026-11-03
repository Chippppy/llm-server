#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="$HOME/.config/llm-server-indicator"
SETUPS_DIR="$CONFIG_DIR/setups"
SUDOERS_FILE="/etc/sudoers.d/llm-server-$USER"
TEMPLATE_UNIT_SRC="$SCRIPT_DIR/systemd/llm-server@.service"
TEMPLATE_UNIT_DEST="/etc/systemd/system/llm-server@.service"
LEGACY_UNIT="/etc/systemd/system/llm-server.service"

DRY_RUN=${DRY_RUN:-0}
export DRY_RUN

# Pre-flight checks
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "Error: $1 is required but not found" >&2; exit 1; }; }
check_cmd cargo
check_cmd sudo
check_cmd systemctl
check_cmd visudo

run_cmd() {
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] $*"
  else
    "$@"
  fi
}

echo "Installing llm-server-indicator (v2)"

# Install binaries via cargo
run_cmd cargo install --path "$SCRIPT_DIR"

# Install both binaries to /usr/local/bin (the template unit's ExecStart and
# the Waybar on-click commands call them by absolute path)
for bin in llm-server-ctl llm-server-indicator; do
  BIN_SRC="${CARGO_HOME:-$HOME/.cargo}/bin/$bin"
  BIN_DEST="/usr/local/bin/$bin"
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] Would install $BIN_SRC to $BIN_DEST"
  else
    if [[ ! -f "$BIN_SRC" ]]; then
      echo "Error: $BIN_SRC not found (was 'cargo install --path' run?)" >&2
      exit 1
    fi
    sudo install -m 0755 "$BIN_SRC" "$BIN_DEST"
    echo "Installed $BIN_DEST"
  fi
done

# Create sudoers file with NOPASSWD for the legacy unit and template instances
SUDOERS_USER="$(whoami)"
SYSTEMCTL_PATH="$(command -v systemctl)"
SUDOERS_CONTENT="$SUDOERS_USER ALL=(ALL) NOPASSWD: $SYSTEMCTL_PATH start llm-server.service, $SYSTEMCTL_PATH stop llm-server.service, $SYSTEMCTL_PATH restart llm-server.service, $SYSTEMCTL_PATH status llm-server.service, $SYSTEMCTL_PATH is-active llm-server.service, $SYSTEMCTL_PATH start llm-server@*.service, $SYSTEMCTL_PATH stop llm-server@*.service, $SYSTEMCTL_PATH restart llm-server@*.service, $SYSTEMCTL_PATH status llm-server@*.service, $SYSTEMCTL_PATH is-active llm-server@*.service"
if [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Would write sudoers file to $SUDOERS_FILE"
  echo "[DRY-RUN] Content: $SUDOERS_CONTENT"
else
  tmpfile=$(mktemp)
  echo "$SUDOERS_CONTENT" > "$tmpfile"
  if sudo visudo -c -f "$tmpfile" >/dev/null 2>&1; then
    sudo mv "$tmpfile" "$SUDOERS_FILE"
    sudo chown root:root "$SUDOERS_FILE"
    sudo chmod 440 "$SUDOERS_FILE"
    echo "Installed sudoers file $SUDOERS_FILE"
  else
    rm -f "$tmpfile"
    echo "Error: sudoers syntax validation failed" >&2
    exit 1
  fi
fi

# Install the template unit (substituting the User= placeholder), then daemon-reload
TARGET_USER="$(whoami)"
if [[ ! -f "$TEMPLATE_UNIT_SRC" ]]; then
  echo "Error: template unit not found at $TEMPLATE_UNIT_SRC" >&2
  exit 1
fi
if [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Would install template unit (User=$TARGET_USER) to $TEMPLATE_UNIT_DEST"
  echo "[DRY-RUN] sudo systemctl daemon-reload"
else
  if [[ -f "$TEMPLATE_UNIT_DEST" ]]; then
    run_cmd sudo cp -p "$TEMPLATE_UNIT_DEST" "${TEMPLATE_UNIT_DEST}.bak.$(date +%s)"
  fi
  tmpunit=$(mktemp)
  sed "s/__INSTALL_USER__/${TARGET_USER}/g" "$TEMPLATE_UNIT_SRC" > "$tmpunit"
  if grep -q "__INSTALL_USER__" "$tmpunit"; then
    rm -f "$tmpunit"
    echo "Error: __INSTALL_USER__ placeholder was not substituted" >&2
    exit 1
  fi
  sudo install -m 0644 "$tmpunit" "$TEMPLATE_UNIT_DEST"
  rm -f "$tmpunit"
  sudo systemctl daemon-reload
  echo "Installed template unit $TEMPLATE_UNIT_DEST"
fi

# Create config directory and install bundled setup templates.
# Templates are starting points only: an existing destination file (from a
# previous install or created by the user) is never backed up or overwritten.
run_cmd mkdir -p "$CONFIG_DIR"
run_cmd mkdir -p "$SETUPS_DIR"
for setup_file in "$SCRIPT_DIR"/setups/*.template.toml; do
  [[ -e "$setup_file" ]] || continue
  setup_dest="$SETUPS_DIR/$(basename "$setup_file")"
  if [[ -f "$setup_dest" ]]; then
    echo "Note: $setup_dest already exists - leaving it untouched."
    continue
  fi
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] Would copy $setup_file to $setup_dest"
  else
    cp "$setup_file" "$setup_dest"
    echo "Installed setup template $setup_dest (fill in the real paths before use)"
  fi
done

# Install the user config only if absent (never overwrite user edits)
if [[ -f "$CONFIG_DIR/config.toml" ]]; then
  echo "Note: $CONFIG_DIR/config.toml already exists - leaving it untouched."
  echo "Merge any entries you need from $SCRIPT_DIR/config/default.toml manually."
elif [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Would copy $SCRIPT_DIR/config/default.toml to $CONFIG_DIR/config.toml"
else
  cp "$SCRIPT_DIR/config/default.toml" "$CONFIG_DIR/config.toml"
  chmod 644 "$CONFIG_DIR/config.toml"
  echo "Installed default config to $CONFIG_DIR/config.toml"
fi

# Legacy v1 unit notice (left untouched; migration is a user-run checkpoint)
if [[ -f "$LEGACY_UNIT" ]]; then
  echo "Note: legacy v1 unit $LEGACY_UNIT is still installed - leaving it untouched."
  echo "Run scripts/migrate_from_v1.sh (user-run checkpoint) to back it up and remove it."
fi

# Optional Waybar integration
INSTALL_WAYBAR=${INSTALL_WAYBAR:-}
if [[ -z "$INSTALL_WAYBAR" ]]; then
  # Interactive prompt if running in a TTY
  if [[ -t 0 && $DRY_RUN -eq 0 ]]; then
    read -rp "Install Waybar integration? [y/N]: " answer
    case "$answer" in
      y|Y|yes|YES) INSTALL_WAYBAR=1 ;;
      *) INSTALL_WAYBAR=0 ;;
    esac
  else
    INSTALL_WAYBAR=0
  fi
fi

if [[ $INSTALL_WAYBAR -eq 1 ]]; then
  if [[ -f "$SCRIPT_DIR/scripts/install-waybar.sh" ]]; then
    echo "Running Waybar installer..."
    if [[ $DRY_RUN -eq 1 ]]; then
      echo "[DRY-RUN] Would run $SCRIPT_DIR/scripts/install-waybar.sh"
    else
      bash "$SCRIPT_DIR/scripts/install-waybar.sh"
    fi
  else
    echo "Warning: Waybar installer not found at $SCRIPT_DIR/scripts/install-waybar.sh" >&2
  fi
else
  echo "Skipping Waybar integration. Run $SCRIPT_DIR/scripts/install-waybar.sh manually if desired."
fi

echo "Installation complete."
echo "Next steps:"
echo "  llm-server-ctl activate --service muse   (or qwen)"
echo "Nothing is enabled or started automatically - v2 has no boot autostart."
