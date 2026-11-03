#!/usr/bin/env bash
set -euo pipefail

# Migrate from the v1 static unit (llm-server.service) to the v2 template unit.
# Explicit, user-run checkpoint: back up the v1 unit, stop + disable it,
# remove the unit file, and reload systemd.
# Idempotent: safe to re-run. Never enables or starts any v2 unit.

LEGACY_UNIT="/etc/systemd/system/llm-server.service"
LEGACY_BACKUP="/etc/systemd/system/llm-server.service.bak-v1"
TEMPLATE_UNIT="/etc/systemd/system/llm-server@.service"

DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)
      DRY_RUN=1
      ;;
    -h|--help)
      cat <<'EOF'
Usage: migrate_from_v1.sh [--dry-run]

Backs up /etc/systemd/system/llm-server.service to
/etc/systemd/system/llm-server.service.bak-v1, stops and disables the v1
unit, removes the unit file, and reloads systemd.

Idempotent: safe to re-run. Never enables or starts any v2 unit.

  --dry-run  Print planned actions without changing anything.
EOF
      exit 0
      ;;
    *)
      echo "Error: unknown argument: $arg" >&2
      echo "Usage: migrate_from_v1.sh [--dry-run]" >&2
      exit 1
      ;;
  esac
done

run_cmd() {
  if [[ $DRY_RUN -eq 1 ]]; then
    echo "[DRY-RUN] $*"
  else
    "$@"
  fi
}

next_steps() {
  cat <<'EOF'
Next steps:
  1. If v2 is not installed yet: run scripts/install.sh
  2. Start a model:  llm-server-ctl activate --service muse   (or qwen)
No v2 unit is enabled or started automatically.
EOF
}

echo "Migrating from v1 unit (dry-run: $DRY_RUN)"

if [[ ! -f "$LEGACY_UNIT" && ! -f "$LEGACY_BACKUP" ]]; then
  echo "No v1 unit found at $LEGACY_UNIT (and no backup at $LEGACY_BACKUP)."
  echo "Nothing to migrate."
  next_steps
  exit 0
fi

# 1. Back up the v1 unit (skip if a backup already exists)
if [[ -f "$LEGACY_BACKUP" ]]; then
  echo "Backup already exists at $LEGACY_BACKUP, skipping backup."
elif [[ -f "$LEGACY_UNIT" ]]; then
  run_cmd sudo cp -p "$LEGACY_UNIT" "$LEGACY_BACKUP"
  [[ $DRY_RUN -eq 1 ]] || echo "Backed up $LEGACY_UNIT to $LEGACY_BACKUP"
else
  echo "Unit file already gone; backup exists at $LEGACY_BACKUP."
fi

# 2. Stop the v1 unit (no-op if not running; note if systemd is unavailable)
run_cmd sudo systemctl stop llm-server.service 2>/dev/null \
  || echo "Note: llm-server.service was not running (or systemd unavailable)"

# 3. Disable the v1 unit (no-op if not enabled)
run_cmd sudo systemctl disable llm-server.service 2>/dev/null \
  || echo "Note: llm-server.service was not enabled (or systemd unavailable)"

# 4. Remove the legacy unit file
run_cmd sudo rm -f "$LEGACY_UNIT"

# 5. Reload systemd
run_cmd sudo systemctl daemon-reload 2>/dev/null \
  || echo "Warning: daemon-reload failed (systemd unavailable?)"

# 6. Verify (real runs only)
if [[ $DRY_RUN -eq 0 ]]; then
  if [[ -f "$LEGACY_UNIT" ]]; then
    echo "Error: $LEGACY_UNIT still exists after removal" >&2
    exit 1
  fi
  if systemctl list-unit-files 'llm-server.service' --no-legend 2>/dev/null | grep -q '^llm-server\.service'; then
    echo "Warning: systemd still lists llm-server.service; run 'sudo systemctl daemon-reload'"
  fi
fi

# 7. Check the v2 template unit is in place (non-fatal)
if [[ ! -f "$TEMPLATE_UNIT" ]]; then
  echo "Note: v2 template unit $TEMPLATE_UNIT is not installed yet."
  echo "Run scripts/install.sh to install it."
fi

echo "Migration complete."
next_steps
