# llm-server

Run and swap multiple local llama.cpp servers (`llama-server`), each with its own model, launch flags, and port. The project provides:

- `llm-server-ctl` — CLI for status/health, start/stop, and exclusive model swapping
- systemd **template units** (`llm-server@<name>.service`) — one instance per setup
- an egui GUI + system tray (`llm-server-indicator --gui`)
- a Waybar module rendering aggregate status

## Sample setup templates

The repo ships two fully commented templates in `setups/` that cover the entire spec schema with placeholder paths and per-model tuned flag defaults:

| Template | Example model | Port | Notable tuned flags |
|---|---|---|---|
| `setups/muse.template.toml` | Muse-Glimmer 30B (`Q4_K_M` + DFlash draft) | 8080 | `temp`/`top_p`/`top_k` sampling |
| `setups/qwen.template.toml` | Qwen 3.8 27B (`Q6_K` + DFlash2 draft) | 8080 | `q8_0` KV cache, medium reasoning effort |

They are **starting points, not ready-to-run specs** — copy one to `~/.config/llm-server-indicator/setups/<name>.toml` and fill in your real paths before use. `llm-server-ctl run` deliberately ignores `*.template.toml` files so an unfilled template can never be launched.

If you install both samples as-is, they land on the same port in the same `group` (`default`), which makes them **exclusive**: activating one stops the other first — a sensible default when two large models can't sit in VRAM at once. Give each of your own setups its own port and group to run them in parallel.

## Architecture

```
llm-server@<name>.service          systemd template unit, one instance per setup
        │  ExecStart=/usr/local/bin/llm-server-ctl run %i
        ▼
llm-server-ctl run <name>          validates setups/<name>.toml (files, binary, port-free pre-flight)
        │  exec()
        ▼
llama-server                       systemd owns the process; SIGTERM on stop goes straight to it
```

- One template unit covers any setup — adding a model = one TOML file + one config entry, no new unit files.
- `Restart=on-failure` so a bad spec trips systemd's rate limit instead of looping every 3 s.
- Per-setup logs: `journalctl -u llm-server@qwen -f`.

## CLI reference

`llm-server-ctl` is installed to `/usr/local/bin/llm-server-ctl`. Global flags:

- `--service NAME` — service name from `config.toml`; required for `start`/`stop`/`toggle`/`activate`; omitted for `status` means *all* services
- `--json` — machine-readable output for Waybar/scripting (conflicts with `--plain`)
- `--plain` — verbose human-readable output (conflicts with `--json`)

| Command | Description |
|---|---|
| `status` | State + health of one service (`--service NAME`) or all services (bare `status` or `--all`, which conflicts with `--service`). Text mode prints a `NAME GROUP STATE HEALTHY PORT` table. |
| `status --service NAME --json` | `{"service","state","healthy","text","tooltip","class"}` |
| `status --all --json` | Aggregate Waybar shape: `{"text","tooltip","class","services":[...]}` — see [Waybar](#waybar) |
| `start --service NAME [--wait] [--timeout N]` | Start via `sudo systemctl`. `--wait` blocks until the health URL answers OK; `--timeout N` seconds (default 300). |
| `stop --service NAME \| stop --all` | Stop one service, or every configured service ("pause it all"). |
| `toggle --service NAME` | Starts the service if stopped/unhealthy (stopping same-group peers first, so an exclusive group never loads two models at once), stops it if running. |
| `activate --service NAME [--no-wait] [--timeout N]` | Exclusive group swap — semantics below. |
| `services` | List configured services (text table `NAME GROUP DISPLAY UNIT`, or `--json` → `{"services":[...]}`). |
| `run <setup>` | **Internal verb**, hidden from `--help` — invoked by the template unit to load the setup TOML and `exec()` the server binary. Not for direct use. |

**`activate` group-swap semantics:** if the target is already active *and* healthy, it is a no-op. Otherwise it stops every other *running* service in the same `group` (stop-first), starts the target, then waits until it reports healthy — unless `--no-wait` — with `--timeout N` seconds (default 300). Services in different groups can run in parallel on different ports.

Action commands (`start`/`stop`/`toggle`/`activate`) print a one-line status, or `{"service","action","status","stopped":[...]}` with `--json` (`stopped` lists side-effect stops and is omitted when empty).

Service states: `active`, `inactive`, `activating`, `deactivating`, `failed`, `unhealthy`, `unknown`, `loading` (systemd-active but the model hasn't finished loading — the port isn't accepting connections yet).

### Examples

```bash
llm-server-ctl status --all                  # table of every setup
llm-server-ctl activate --service qwen       # stops muse if running, starts qwen, waits for healthy
llm-server-ctl stop --all                    # pause everything
llm-server-ctl start --service muse --wait --timeout 300
llm-server-ctl status --service qwen --json  # single-service JSON
```

`llm-server-indicator` is the GUI binary but also delegates to the CLI: `llm-server-indicator --gui` opens the egui window/tray, any other arguments behave like `llm-server-ctl`.

## Configuration

### Service config — `~/.config/llm-server-indicator/config.toml`

```toml
[services.<name>]
unit = "llm-server@<name>.service"          # systemd unit to control
display_name = "Qwen 3.8 27B"               # shown in status text, GUI, Waybar
health_url = "http://127.0.0.1:8080/health" # polled for health
group = "default"                           # exclusive swap group (default: "default")
```

Services sharing a `group` are swapped exclusively by `activate`. If the user config is absent, the repo's `config/default.toml` is used as a fallback (the installer copies it for you).

### Setup specs — `~/.config/llm-server-indicator/setups/<name>.toml`

One TOML per setup. Before `exec()`-ing, `run` validates the spec (binary exists and is executable, model files exist, port ≠ 0, `draft` requires `spec_type`, `spec_draft_n_max` requires `draft`) and pre-flights that the port is not already bound. An example based on `setups/qwen.template.toml` (placeholder paths):

```toml
[server]
binary = "/path/to/llama.cpp/build/bin/llama-server"
host = "127.0.0.1"
port = 8080

[model]
main = "/path/to/models/qwen-3.8/Qwen3.8-27B-Q6_K.gguf"
draft = "/path/to/models/qwen-3.8/Qwen3.8-27B-DFlash2-Q8_0.gguf"
spec_type = "draft-dflash"
spec_draft_n_max = 4

[flags]
ngl = 99
ngld = 99
ctx_size = 131072
flash_attention = true
jinja = true
cache_type_k = "q8_0"
cache_type_v = "q8_0"
extra_args = ["-np", "1", "--no-reasoning-preserve", "--chat-template-kwargs", "{\"reasoning_effort\":\"medium\"}"]
```

Supported fields:

| Section | Field | Required | llama-server flag |
|---|---|---|---|
| `[server]` | `binary` | yes | — (the executable) |
| | `host` | no (default `127.0.0.1`) | `--host` |
| | `port` | yes | `--port` |
| `[model]` | `main` | yes | `-m` |
| | `draft` | no | `-md` |
| | `spec_type` | if `draft` set | `--spec-type` |
| | `spec_draft_n_max` | no (needs `draft`) | `--spec-draft-n-max` |
| `[flags]` | `ngl` / `ngld` | no | `-ngl` / `-ngld` |
| | `ctx_size` | no | `-c` |
| | `flash_attention` | no (`true`/`false`) | `-fa on` / `-fa off` |
| | `jinja` | no (emitted when `true`) | `--jinja` |
| | `temp` / `top_p` / `top_k` | no | `--temp` / `--top-p` / `--top-k` |
| | `cache_type_k` / `cache_type_v` | no | `--cache-type-k` / `--cache-type-v` |
| | `extra_args` | no (array) | appended verbatim — escape hatch for anything the schema doesn't cover |

The setups directory can be overridden with the `LLM_SERVER_INDICATOR_SETUPS_DIR` env var.

### Adding a new model / setup

1. Copy `setups/qwen.template.toml` (or `setups/muse.template.toml`) to `~/.config/llm-server-indicator/setups/<name>.toml` and fill in your paths.
2. Add a `[services.<name>]` entry to `config.toml` with the new `unit` (`llm-server@<name>.service`), `display_name`, `health_url` on a new port, and a `group`.
3. `llm-server-ctl activate --service <name>`.

No new systemd unit file is needed — the template instantiates for any name. Names are validated as short alphanumeric (`^[A-Za-z0-9._@-]+$`; the setup file must be a plain file stem). Give the new setup its **own `group`** if you want it to run concurrently with `muse`/`qwen` (it must also use its own port).

## Installation

```bash
git clone https://github.com/Chippppy/llm-server llm-server
cd llm-server
./scripts/install.sh                  # preview: DRY_RUN=1 ./scripts/install.sh
```

`scripts/install.sh` (requires `cargo`, `sudo`, `systemctl`, `visudo`):

1. `cargo install --path .` (builds `llm-server-ctl` and `llm-server-indicator`)
2. Installs `llm-server-ctl` to `/usr/local/bin/llm-server-ctl` — the stable absolute path the template unit's `ExecStart` calls
3. Writes `/etc/sudoers.d/llm-server-$USER` granting NOPASSWD `systemctl start|stop|restart|status|is-active` for `llm-server.service` and `llm-server@*.service` (validated with `visudo -c`, written atomically) — review it, and delete it if you don't need passwordless control
4. Installs `systemd/llm-server@.service` to `/etc/systemd/system/` with `User=` substituted to the installing user (existing unit backed up with a timestamp), then `daemon-reload`
5. Copies `setups/*.template.toml` → `~/.config/llm-server-indicator/setups/` only if the destination file does not already exist (never clobbers user specs)
6. Copies `config/default.toml` → `~/.config/llm-server-indicator/config.toml` **only if it doesn't already exist** (never overwrites user edits)
7. Optionally runs the Waybar installer (TTY prompt, or set `INSTALL_WAYBAR=1`)

**Nothing is enabled or started.** v2 deliberately has no boot autostart — start what you need with `llm-server-ctl activate`, and opt in per setup with `sudo systemctl enable llm-server@<name>.service` if you want one running at boot.

### Waybar

```bash
./scripts/install-waybar.sh           # preview: DRY_RUN=1 ./scripts/install-waybar.sh
```

Writes `waybar/llm-server-module.json` to `~/.config/waybar/modules/llm-server-module.json` (existing file backed up to `.bak`) and appends `waybar/style.css` to `~/.config/waybar/style.css`. It does **not** modify your Waybar config — the config must import the module file and list `custom/llm-server` in a modules array; the script warns if it doesn't see that.

### Uninstall

```bash
./scripts/uninstall.sh
```

Stops running `llm-server@*.service` instances, removes the template unit, `/usr/local/bin/llm-server-ctl`, the sudoers file, and `~/.config/llm-server-indicator` (plus the v1 autostart desktop entry if present). The Waybar integration and any legacy v1 unit are left in place on purpose.

## Migrating from v1

Existing users of the legacy single-server unit: after running `scripts/install.sh`, run `./scripts/migrate_from_v1.sh` (preview with `--dry-run`). It backs up `/etc/systemd/system/llm-server.service`, stops and disables it, removes the unit file, and reloads systemd. It is idempotent and never enables or starts any v2 unit.

## Waybar

The Waybar integration is a **drawer group** (`group/llm`) in the style of Waybar's sliding group modules: the aggregate status is always visible, and hovering over it slides out one button per model.

- `waybar/llm-server-module.json` — the group **leader**. Runs `llm-server-ctl status --all --json` every 5 s and renders the aggregate JSON directly (`return-type: "json`):
  - **text** — the active setup with an icon (e.g. `󰍲 Qwen 3.8 27B`), or `󰐇 LLM off` when nothing is running
  - **tooltip** — one line per service: `<display name>: <state>`
  - **class** — `active` / `loading` / `unhealthy` / `inactive` (any failed/unhealthy service wins, then any activating/deactivating, then any active)
  - left click opens the indicator GUI (`llm-server-indicator --gui`)
- `waybar/group-llm.json` — the `group/llm` drawer (slides out on hover) plus two per-model buttons. Each button runs `llm-server-ctl status --service <name> --json` so it shows that model's live state, color-coded by state class.

The bundled `group-llm.json` ships with two per-model buttons (`muse` and `qwen`) matching the sample templates above; rename them (and the `--service NAME` values) to match your own setups. Button gestures (services in the same exclusive group are stopped first when toggling a peer on):

| Button | Left click | Right click |
|---|---|---|
| `muse` | Toggle `muse`: start (stopping same-group peers) or stop (`toggle --service muse`) | Stop `muse` (`stop --service muse`) |
| `qwen` | Toggle `qwen`: start (stopping same-group peers) or stop (`toggle --service qwen`) | Stop `qwen` (`stop --service qwen`) |

Styling lives in `waybar/style.css`, keyed on the state classes. If you rename services, update the `--service NAME` values in both `waybar/llm-server-module.json` and `waybar/group-llm.json`.

`scripts/install-waybar.sh` copies the module and group files, patches `~/.config/waybar/config.jsonc` idempotently (with backups): it adds the group include next to the module include and lists `group/llm` in the modules array in place of `custom/llm-server`. The project `style.css` is appended to your `~/.config/waybar/style.css` behind a marker; re-running the installer replaces the previous llm block instead of duplicating it.

## Development

```bash
cargo build
cargo test                          # unit tests + hermetic integration tests (tests/integration_test.rs)
scripts/test_install_dry_run.sh     # asserts install.sh dry-run output
```

Integration tests are hermetic: they point `XDG_CONFIG_HOME` at a temporary fixture so a machine's real config can't leak into the tests.
