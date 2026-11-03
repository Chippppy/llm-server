#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_SCRIPT="$SCRIPT_DIR/scripts/install.sh"

echo "Testing install script dry-run..."

# Run install script in dry-run mode, capture output
OUTPUT=$(DRY_RUN=1 INSTALL_WAYBAR=1 bash "$INSTALL_SCRIPT" 2>&1)

fail() {
  echo "FAIL: $1" >&2
  echo "----- install.sh dry-run output -----"
  echo "$OUTPUT"
  exit 1
}

if echo "$OUTPUT" | grep -q "\[DRY-RUN\] cargo install"; then
  echo "PASS: cargo install dry-run detected"
else
  fail "cargo install dry-run not found"
fi

if echo "$OUTPUT" | grep -q "Would write sudoers file"; then
  echo "PASS: sudoers dry-run detected"
else
  fail "sudoers dry-run not found"
fi

if echo "$OUTPUT" | grep -q "Would install.*llm-server-ctl"; then
  echo "PASS: llm-server-ctl install dry-run detected"
else
  fail "llm-server-ctl install dry-run not found"
fi

if echo "$OUTPUT" | grep -q "Would install template unit"; then
  echo "PASS: template unit dry-run detected"
else
  fail "template unit dry-run not found"
fi

if echo "$OUTPUT" | grep -q "\[DRY-RUN\] sudo systemctl daemon-reload"; then
  echo "PASS: daemon-reload dry-run detected"
else
  fail "daemon-reload dry-run not found"
fi

# The installer only copies bundled *.template.toml files (skipping any that
# already exist), so the dry-run must announce the template basenames.
if echo "$OUTPUT" | grep -q "Would copy.*setups/muse.template.toml"; then
  echo "PASS: setups template copy dry-run detected"
else
  fail "setups template copy dry-run not found"
fi

# On a clean machine (CI) the installer copies the default config; on a
# machine with an existing user config it leaves it untouched.
if echo "$OUTPUT" | grep -qE "Would copy.*default\.toml|config\.toml already exists"; then
  echo "PASS: config handling dry-run detected"
else
  fail "config handling dry-run not found"
fi

if echo "$OUTPUT" | grep -q "Would run.*install-waybar.sh"; then
  echo "PASS: Waybar installer dry-run detected"
else
  fail "Waybar installer dry-run not found"
fi

echo "All dry-run checks passed."
