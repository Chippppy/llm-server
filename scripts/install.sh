#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG_DIR="$HOME/.config/llm-server-indicator"
SETUPS_DIR="$CONFIG_DIR/setups"
# whoami, not $USER (the env var is spoofable and could inject path separators
# into the sudoers file name)
INSTALL_USER="$(whoami)"
# sudo only HONORS /etc/sudoers.d filenames matching [a-zA-Z0-9_-]+ and
# silently IGNORES any other name (e.g. a username containing '.'). Sanitize
# to that charset so a nonconforming username can't make the grant vanish
# while the install "succeeds". uninstall.sh derives the same name.
SUDOERS_NAME="$(printf 'llm-server-%s' "$INSTALL_USER" | tr -c 'a-zA-Z0-9_' '-')"
if [[ "$SUDOERS_NAME" != "llm-server-$INSTALL_USER" ]]; then
  echo "Warning: username '$INSTALL_USER' contains characters sudo ignores in sudoers.d filenames; using sanitized name '$SUDOERS_NAME'" >&2
fi
SUDOERS_FILE="/etc/sudoers.d/$SUDOERS_NAME"
TEMPLATE_UNIT_SRC="$SCRIPT_DIR/systemd/llm-server@.service"
TEMPLATE_UNIT_DEST="/etc/systemd/system/llm-server@.service"
LEGACY_UNIT="/etc/systemd/system/llm-server.service"

DRY_RUN=${DRY_RUN:-0}
export DRY_RUN

# Validate INSTALL_WAYBAR up front: a non-numeric value would otherwise abort
# under set -e mid-install, after privileged steps had already run.
case "${INSTALL_WAYBAR:-}" in
  '' | 0 | 1) ;;
  *)
    echo "Error: INSTALL_WAYBAR must be 0 or 1 (got '$INSTALL_WAYBAR')" >&2
    exit 1
    ;;
esac

# Pre-flight checks
check_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "Error: $1 is required but not found" >&2; exit 1; }; }
check_cmd cargo
check_cmd sudo
check_cmd systemctl
check_cmd visudo

# The NOPASSWD grant below uses regex command arguments, which require
# sudo >= 1.9.10 (older versions only understand plain wildcard arguments).
# Parse the first line, e.g. "Sudo version 1.9.17p2".
SUDO_VERSION="$(sudo --version | head -n1 | sed -n 's/^Sudo version \([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')"
if [[ -z "$SUDO_VERSION" ]]; then
  echo "Error: could not parse sudo version (expected 'Sudo version X.Y.Z', got: $(sudo --version | head -n1))" >&2
  exit 1
fi
# sort -C -V: exit 0 iff the pair (minimum, found) is version-sorted,
# i.e. found >= minimum.
if ! printf '%s\n%s\n' "1.9.10" "$SUDO_VERSION" | sort -C -V; then
  echo "Error: sudo >= 1.9.10 is required (found $SUDO_VERSION): the sudoers grant uses anchored regex command arguments, which older sudo versions do not support." >&2
  exit 1
fi

run_cmd() {
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] $*"
  else
    "$@"
  fi
}

echo "Installing llm-server-indicator (v2)"

# Install binaries via cargo (--locked: reproducible build from Cargo.lock)
run_cmd cargo install --path "$SCRIPT_DIR" --locked

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
# (no `restart`: it is the persistence-enabler — a compromised user could keep
# a malicious setup alive without a password; `stop` + `start` covers the need)
#
# sudoers command arguments are matched as ONE concatenated string, and `*`
# matches whitespace: an unanchored `llm-server@*.service` would also match
# "llm-server@x.service /tmp/evil.service", letting a local user run an
# arbitrary unit as root. So each template-instance verb gets an anchored
# extended-regex argument (sudo >= 1.9.10): the first argument starts with ^
# and the pattern ends with $, so the whole user argv must match exactly —
# one verb, one unit, no extra arguments. The unit-name charset mirrors
# validate_setup_name (src/run.rs). The legacy llm-server.service entries
# contain no wildcards, so plain exact strings are safe and stay as-is.
SYSTEMCTL_PATH="$(command -v systemctl)"
case "$SYSTEMCTL_PATH" in
  /usr/bin/* | /bin/*) ;;
  *)
    echo "Error: systemctl resolved to $SYSTEMCTL_PATH; refusing to bake a sudoers grant for a path outside /usr/bin or /bin" >&2
    exit 1
    ;;
esac
LEGACY_UNIT_GRANT="llm-server.service"
TEMPLATE_UNIT_RE='llm-server@[A-Za-z0-9._-]+\.service'
# The user-spec tag prefix is mandatory: without it the lines are not valid
# sudoers (visudo -c rejects them), so the installer can never complete.
SUDOERS_CONTENT="$INSTALL_USER ALL=(ALL) NOPASSWD: "
for verb in start stop status is-active; do
  SUDOERS_CONTENT+="$SYSTEMCTL_PATH $verb $LEGACY_UNIT_GRANT, "
  SUDOERS_CONTENT+="$SYSTEMCTL_PATH ^$verb ${TEMPLATE_UNIT_RE}\$, "
done
SUDOERS_CONTENT="${SUDOERS_CONTENT%, }"
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
  # Escape sed replacement metacharacters in the username before interpolating
  # it into the replacement string (defense in depth; whoami is normally safe).
  escaped="${TARGET_USER//\\/\\\\}"
  escaped="${escaped//&/\\&}"
  escaped="${escaped//|/\\|}"
  sed "s|__INSTALL_USER__|${escaped}|g" "$TEMPLATE_UNIT_SRC" > "$tmpunit"
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
# Mode 700 on both dirs: the default umask (755) would let any local user
# enumerate setup names under $SETUPS_DIR.
if [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Would create $CONFIG_DIR and $SETUPS_DIR (mode 700)"
else
  mkdir -p "$CONFIG_DIR" "$SETUPS_DIR"
  chmod 700 "$CONFIG_DIR" "$SETUPS_DIR"
fi
for setup_file in "$SCRIPT_DIR"/setups/*.template.toml; do
  [[ -e "$setup_file" ]] || continue
  setup_dest="$SETUPS_DIR/$(basename "$setup_file")"
  if [[ -f "$setup_dest" ]]; then
    echo "Note: $setup_dest already exists - leaving it untouched."
    continue
  fi
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] Would copy $setup_file to $setup_dest (mode 600)"
  else
    install -m 600 "$setup_file" "$setup_dest"
    echo "Installed setup template $setup_dest (fill in the real paths before use)"
  fi
done

# Install the user config only if absent (never overwrite user edits)
if [[ -f "$CONFIG_DIR/config.toml" ]]; then
  echo "Note: $CONFIG_DIR/config.toml already exists - leaving it untouched."
  echo "Merge any entries you need from $SCRIPT_DIR/config/default.toml manually."
elif [[ $DRY_RUN -eq 1 ]]; then
  echo "[DRY-RUN] Would copy $SCRIPT_DIR/config/default.toml to $CONFIG_DIR/config.toml (mode 600)"
else
  install -m 600 "$SCRIPT_DIR/config/default.toml" "$CONFIG_DIR/config.toml"
  echo "Installed default config to $CONFIG_DIR/config.toml"
fi

# Legacy v1 unit notice (left untouched; migration is a user-run checkpoint)
if [[ -f "$LEGACY_UNIT" ]]; then
  echo "Note: legacy v1 unit $LEGACY_UNIT is still installed - leaving it untouched."
  echo "Run scripts/migrate_from_v1.sh (user-run checkpoint) to back it up and remove it."
fi

# Optional Waybar integration (INSTALL_WAYBAR validated up front; may be unset)
if [[ -z "${INSTALL_WAYBAR:-}" ]]; then
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
