//! CLI argument parsing and command dispatch

use clap::{Parser, Subcommand};
use serde::Serialize;
use std::time::Duration;

use crate::config::{load_config, Config};
use crate::lock::ProcessLock;
use crate::service::{ServiceController, ServiceState};

#[derive(Parser, Debug)]
#[command(
    name = "llm-server-ctl",
    about = "Control and monitor systemd LLM services from the command line",
    long_about = "llm-server-ctl is a small CLI for managing systemd services like llm-server@muse.service. It can query systemd state, check HTTP health, start/stop/toggle services, swap between model setups (activate), and emit Waybar-compatible JSON.

The tool reads service definitions from ~/.config/llm-server-indicator/config.toml with fallback to config/default.toml.

Use --json for machine-readable output suitable for Waybar custom modules.",
    version,
    author = "Chippppy",
    arg_required_else_help = false,
    after_help = "Examples:
  llm-server-ctl status --service muse --json
  llm-server-ctl status --all --json
  llm-server-ctl start --service muse --wait --timeout 300
  llm-server-ctl stop --all
  llm-server-ctl activate --service qwen
  llm-server-ctl services

Waybar usage:
  exec: llm-server-ctl status --all --json
  on-click: llm-server-ctl activate --service qwen"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(
        long,
        global = true,
        help = "Service name as defined in config.toml (required for start/stop/toggle/activate; omitted for status means all services)",
        value_name = "NAME"
    )]
    pub service: Option<String>,

    #[arg(
        long,
        global = true,
        conflicts_with = "plain",
        help = "Output in JSON format for Waybar / scripting"
    )]
    pub json: bool,

    #[arg(
        long,
        global = true,
        conflicts_with = "json",
        help = "Output in human-readable plain text"
    )]
    pub plain: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Show current status of a service (all services if --service is omitted)
    ///
    /// Prints systemd state and HTTP health. Use --json for Waybar.
    /// Example: llm-server-ctl status --service muse --json
    Status {
        /// All services (same as omitting --service)
        #[arg(long, conflicts_with = "service")]
        all: bool,
    },

    /// Start a service via sudo systemctl
    ///
    /// Example: llm-server-ctl start --service muse
    Start {
        /// Wait until the service reports healthy before returning
        #[arg(long)]
        wait: bool,
        /// Seconds to wait for health when using --wait (default: 300)
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },

    /// Stop a service via sudo systemctl
    ///
    /// Example: llm-server-ctl stop --service muse
    /// Use --all to stop every configured service ("pause it all").
    Stop {
        /// Stop every configured service
        #[arg(long, conflicts_with = "service")]
        all: bool,
    },

    /// Toggle service running state
    ///
    /// Starts the service if stopped/unhealthy, stops it if running.
    /// Example: llm-server-ctl toggle --service muse
    Toggle,

    /// Make this service the active one in its group
    ///
    /// Stops the other running services in the same group, then starts this
    /// one and waits until it is healthy.
    /// Example: llm-server-ctl activate --service qwen
    Activate {
        /// Don't wait for the service to become healthy after starting
        #[arg(long)]
        no_wait: bool,
        /// Seconds to wait for health (default: 300)
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },

    /// List all configured services
    ///
    /// Example: llm-server-ctl services --json
    Services,

    /// [internal] Load a setup spec and exec the server binary (invoked by the systemd template unit)
    #[command(hide = true)]
    Run {
        /// Setup name (file stem in the setups dir), e.g. muse
        setup: String,
    },
}

#[derive(Serialize, Debug)]
struct StatusOutput {
    service: String,
    state: String,
    healthy: bool,
    text: String,
    tooltip: String,
    class: String,
}

/// Aggregate (all-services) Waybar status. Top-level text/tooltip/class keys
/// so a Waybar module with `return-type: json` renders it directly.
#[derive(Serialize, Debug)]
struct AggregateStatusOutput {
    text: String,
    tooltip: String,
    class: String,
    services: Vec<StatusOutput>,
}

#[derive(Serialize, Debug)]
struct ActionOutput {
    service: String,
    action: String,
    status: String,
    /// Services stopped as a side effect (stop --all, activate). Omitted
    /// when empty so the v1 JSON shape is preserved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stopped: Vec<String>,
}

#[derive(Serialize, Debug)]
struct ServicesOutput {
    services: Vec<String>,
}

/// One configured service with its current combined state, used to build
/// the aggregate (all-services) status view.
struct ServiceRow {
    name: String,
    group: String,
    display: String,
    state: ServiceState,
    healthy: bool,
    health_url: String,
}

fn state_to_string(state: &ServiceState) -> &'static str {
    match state {
        ServiceState::Active => "active",
        ServiceState::Inactive => "inactive",
        ServiceState::Activating => "activating",
        ServiceState::Deactivating => "deactivating",
        ServiceState::Failed => "failed",
        ServiceState::Unknown => "unknown",
        ServiceState::Unhealthy => "unhealthy",
        ServiceState::Loading => "loading",
    }
}

fn state_icon(state: &ServiceState) -> &'static str {
    match state {
        ServiceState::Active => "󰍲",
        ServiceState::Inactive => "󰐇",
        ServiceState::Activating => "⟳",
        ServiceState::Deactivating => "⟳",
        ServiceState::Failed => "󰅖",
        ServiceState::Unknown => "󰙏",
        ServiceState::Unhealthy => "⚠️",
        ServiceState::Loading => "⟳",
    }
}

fn format_status_output(
    service_name: &str,
    service_display: &str,
    state: &ServiceState,
    healthy: bool,
) -> StatusOutput {
    let state_str = state_to_string(state);
    let icon = state_icon(state);
    let text = format!("{} {}", icon, service_display);
    let class = state_str.to_string();
    let tooltip = format!("{} - {}", service_display, state_str);
    StatusOutput {
        service: service_name.to_string(),
        state: state_str.to_string(),
        healthy,
        text,
        tooltip,
        class,
    }
}

fn get_controller(service_name: &str) -> Result<ServiceController, String> {
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    controller_for(&cfg, service_name)
}

fn controller_for(cfg: &Config, service_name: &str) -> Result<ServiceController, String> {
    cfg.services
        .get(service_name)
        .cloned()
        .map(|svc| ServiceController::new(svc, None))
        .ok_or_else(|| format!("Service '{}' not found in config", service_name))
}

fn validate_service_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
    {
        Err(format!(
            "Invalid service name '{}': must match ^[A-Za-z0-9._@-]+$",
            name
        ))
    } else {
        Ok(())
    }
}

pub fn run() -> Result<(), String> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Status { .. } => handle_status(&cli),
        Commands::Start { wait, timeout } => handle_start(&cli, wait, timeout),
        Commands::Stop { .. } => handle_stop(&cli),
        Commands::Toggle => handle_toggle(&cli),
        Commands::Activate { no_wait, timeout } => handle_activate(&cli, no_wait, timeout),
        Commands::Services => handle_services(&cli),
        Commands::Run { setup } => crate::run::run_setup(&setup),
    }
}

fn handle_status(cli: &Cli) -> Result<(), String> {
    let all = matches!(cli.command, Commands::Status { all: true });
    if all && cli.service.is_some() {
        // clap normally rejects this via conflicts_with; guard defensively.
        return Err("status: --all cannot be combined with --service".to_string());
    }
    match cli.service.as_deref() {
        Some(service_name) => handle_single_status(service_name, cli),
        None => handle_aggregate_status(cli),
    }
}

fn handle_single_status(service_name: &str, cli: &Cli) -> Result<(), String> {
    validate_service_name(service_name)?;
    let controller = get_controller(service_name)?;
    let (state, healthy) = controller.combined_state();
    let display_name = controller.service_config.display_name.clone();
    let output = format_status_output(service_name, &display_name, &state, healthy);

    if cli.json {
        let json = serde_json::to_string(&output).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else if cli.plain {
        println!(
            "{}: {} (healthy={})",
            service_name, output.state, output.healthy
        );
        println!("text: {}", output.text);
        println!("tooltip: {}", output.tooltip);
    } else {
        println!("{}", output.text);
        println!("{}", output.tooltip);
    }
    Ok(())
}

fn handle_aggregate_status(cli: &Cli) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    let mut names: Vec<&String> = cfg.services.keys().collect();
    names.sort();

    let mut rows: Vec<ServiceRow> = Vec::with_capacity(names.len());
    for name in names {
        let svc = &cfg.services[name];
        let controller = ServiceController::new(svc.clone(), None);
        let (state, healthy) = controller.combined_state();
        rows.push(ServiceRow {
            name: name.clone(),
            group: svc.group.clone(),
            display: svc.display_name.clone(),
            state,
            healthy,
            health_url: svc.health_url.clone(),
        });
    }

    if cli.json {
        let services: Vec<StatusOutput> = rows
            .iter()
            .map(|r| format_status_output(&r.name, &r.display, &r.state, r.healthy))
            .collect();
        let entries: Vec<(String, ServiceState, bool)> = rows
            .iter()
            .map(|r| (r.display.clone(), r.state.clone(), r.healthy))
            .collect();
        let (text, tooltip, class) = aggregate_waybar(&entries);
        let out = AggregateStatusOutput {
            text,
            tooltip,
            class,
            services,
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else {
        let table_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.name.clone(),
                    r.group.clone(),
                    state_to_string(&r.state).to_string(),
                    if r.healthy {
                        "yes".to_string()
                    } else {
                        "no".to_string()
                    },
                    port_cell(&r.health_url),
                ]
            })
            .collect();
        println!(
            "{}",
            render_table(&["NAME", "GROUP", "STATE", "HEALTHY", "PORT"], &table_rows)
        );
    }
    Ok(())
}

fn handle_start(cli: &Cli, wait: bool, timeout: u64) -> Result<(), String> {
    let service_name = cli
        .service
        .as_deref()
        .ok_or_else(|| "Missing required --service argument for start".to_string())?;
    validate_service_name(service_name)?;
    let controller = get_controller(service_name)?;
    controller
        .start()
        .map_err(|e| format!("Failed to start service: {:?}", e))?;
    if wait {
        if !controller.wait_until_healthy(Duration::from_secs(timeout)) {
            return Err(format!(
                "Service {} started but was not healthy within {} s",
                service_name, timeout
            ));
        }
    }
    if cli.json {
        let out = ActionOutput {
            service: service_name.to_string(),
            action: "start".to_string(),
            status: "ok".to_string(),
            stopped: Vec::new(),
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else if wait {
        println!("Service {} started and is healthy", service_name);
    } else {
        println!("Service {} started", service_name);
    }
    Ok(())
}

fn handle_stop(cli: &Cli) -> Result<(), String> {
    if let Some(service_name) = cli.service.as_deref() {
        return handle_single_stop(service_name, cli);
    }
    let all = matches!(cli.command, Commands::Stop { all: true });
    if !all {
        // clap normally rejects --all + --service via conflicts_with; this
        // covers a bare `stop` and any conflict that slips through.
        return Err("stop requires --service or --all".to_string());
    }
    handle_stop_all(cli)
}

fn handle_single_stop(service_name: &str, cli: &Cli) -> Result<(), String> {
    validate_service_name(service_name)?;
    let controller = get_controller(service_name)?;
    controller
        .stop()
        .map_err(|e| format!("Failed to stop service: {:?}", e))?;
    if cli.json {
        let out = ActionOutput {
            service: service_name.to_string(),
            action: "stop".to_string(),
            status: "ok".to_string(),
            stopped: Vec::new(),
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else {
        println!("Service {} stopped", service_name);
    }
    Ok(())
}

fn handle_stop_all(cli: &Cli) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    // Serialize against concurrent `activate`/`stop --all` runs.
    let _lock = ProcessLock::acquire().map_err(|e| format!("stop --all: {}", e))?;
    let mut names: Vec<&String> = cfg.services.keys().collect();
    names.sort();

    let mut stopped: Vec<String> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    for name in names {
        let controller = controller_for(&cfg, name)?;
        match controller.stop() {
            Ok(()) => stopped.push(name.clone()),
            Err(e) => failures.push((name.clone(), format!("{:?}", e))),
        }
    }

    let status = if failures.is_empty() { "ok" } else { "partial" };
    if cli.json {
        let out = ActionOutput {
            service: "all".to_string(),
            action: "stop".to_string(),
            status: status.to_string(),
            stopped: stopped.clone(),
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    }

    if failures.is_empty() {
        if !cli.json {
            println!("Stopped: {}", stopped_or_none(&stopped));
        }
        Ok(())
    } else {
        let failed = failures
            .iter()
            .map(|(n, e)| format!("{} ({})", n, e))
            .collect::<Vec<_>>()
            .join("; ");
        Err(format!(
            "Stopped {} but failed to stop: {}",
            stopped_or_none(&stopped),
            failed
        ))
    }
}

fn handle_toggle(cli: &Cli) -> Result<(), String> {
    let service_name = cli
        .service
        .as_deref()
        .ok_or_else(|| "Missing required --service argument for toggle".to_string())?;
    validate_service_name(service_name)?;
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    let controller = controller_for(&cfg, service_name)?;
    let (current_state, _) = controller.combined_state();
    let (status, stopped) = if toggle_is_stop(&current_state) {
        controller
            .stop()
            .map_err(|e| format!("Failed to stop service: {:?}", e))?;
        ("stopped", Vec::new())
    } else {
        // Exclusive groups: stop same-group peers before starting, exactly
        // like `activate`, so two models in one group never load at once.
        let mut entries: Vec<(String, String, ServiceState)> = Vec::new();
        for (name, svc) in &cfg.services {
            let peer = controller_for(&cfg, name)?;
            let (state, _) = peer.combined_state();
            entries.push((name.clone(), svc.group.clone(), state));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut stopped = Vec::new();
        for name in members_to_stop(service_name, &entries) {
            let peer = controller_for(&cfg, &name)?;
            peer.stop().map_err(|e| {
                format!(
                    "Failed to stop '{}' before toggling '{}': {:?}",
                    name, service_name, e
                )
            })?;
            stopped.push(name);
        }
        controller
            .start()
            .map_err(|e| format!("Failed to start service: {:?}", e))?;
        ("started", stopped)
    };
    if cli.json {
        let out = ActionOutput {
            service: service_name.to_string(),
            action: "toggle".to_string(),
            status: status.to_string(),
            stopped,
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else {
        println!("Service {} {}", service_name, status);
        if !stopped.is_empty() {
            println!("Stopped: {}", stopped.join(", "));
        }
    }
    Ok(())
}

fn handle_activate(cli: &Cli, no_wait: bool, timeout: u64) -> Result<(), String> {
    let service_name = cli
        .service
        .as_deref()
        .ok_or_else(|| "Missing required --service argument for activate".to_string())?;
    validate_service_name(service_name)?;
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    if !cfg.services.contains_key(service_name) {
        return Err(format!("Service '{}' not found in config", service_name));
    }

    // Serialize the whole stop→start→wait sequence against other mutating
    // commands (double-activate, concurrent `stop --all`); held until return.
    let _lock = ProcessLock::acquire().map_err(|e| format!("activate: {}", e))?;

    // (name, group, combined_state) for every configured service, sorted.
    let mut entries: Vec<(String, String, ServiceState)> = Vec::new();
    for (name, svc) in &cfg.services {
        let controller = controller_for(&cfg, name)?;
        let (state, _) = controller.combined_state();
        entries.push((name.clone(), svc.group.clone(), state));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // combined_state is Active only when systemd reports active AND the
    // health check passes, so Active already means "active and healthy" —
    // only that is a no-op. A target already Loading or Activating is NOT
    // a no-op: re-activation proceeds through the normal path (stop
    // same-group peers, systemctl start as a no-op, then wait for health),
    // so a stuck Loading target (port never opens) stays re-activatable
    // and we wait for the load to finish instead of lying.
    let target_state = entries
        .iter()
        .find(|(name, _, _)| name == service_name)
        .map(|(_, _, state)| state.clone());
    if activate_is_noop(target_state.as_ref()) {
        let message = format!("Service {} is already active and healthy", service_name);
        let out = ActionOutput {
            service: service_name.to_string(),
            action: "activate".to_string(),
            status: "ok".to_string(),
            stopped: Vec::new(),
        };
        return print_action(&out, cli, &message);
    }

    let to_stop = members_to_stop(service_name, &entries);
    let mut stopped: Vec<String> = Vec::new();
    for name in to_stop {
        let controller = controller_for(&cfg, &name)?;
        if let Err(e) = controller.stop() {
            // Never start the target if a peer could not be stopped.
            return Err(emit_activate_error(
                cli,
                service_name,
                "failed",
                &stopped,
                format!(
                    "Failed to stop '{}' before activating '{}': {:?} (stopped: {})",
                    name,
                    service_name,
                    e,
                    stopped_or_none(&stopped)
                ),
            ));
        }
        stopped.push(name);
    }

    let controller = controller_for(&cfg, service_name)?;
    if let Err(e) = controller.start() {
        return Err(emit_activate_error(
            cli,
            service_name,
            "failed",
            &stopped,
            format!(
                "Failed to start '{}': {:?} (stopped: {})",
                service_name,
                e,
                stopped_or_none(&stopped)
            ),
        ));
    }

    let status = if no_wait {
        "started"
    } else if !controller.wait_until_healthy(Duration::from_secs(timeout)) {
        return Err(emit_activate_error(
            cli,
            service_name,
            "timeout",
            &stopped,
            format!(
                "Timed out waiting for '{}' to become healthy within {} s (stopped: {})",
                service_name,
                timeout,
                stopped_or_none(&stopped)
            ),
        ));
    } else {
        "ok"
    };

    let mut text = if no_wait {
        format!("Service {} started (skipping health wait)", service_name)
    } else {
        format!("Service {} activated", service_name)
    };
    if !stopped.is_empty() {
        text.push_str(&format!(" (stopped: {})", stopped.join(", ")));
    }
    let out = ActionOutput {
        service: service_name.to_string(),
        action: "activate".to_string(),
        status: status.to_string(),
        stopped,
    };
    print_action(&out, cli, &text)
}

fn handle_services(cli: &Cli) -> Result<(), String> {
    let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
    let mut names: Vec<&String> = cfg.services.keys().collect();
    names.sort();
    if cli.json {
        let out = ServicesOutput {
            services: names.iter().map(|s| s.to_string()).collect(),
        };
        let json = serde_json::to_string(&out).map_err(|e| e.to_string())?;
        println!("{}", json);
    } else {
        let table_rows: Vec<Vec<String>> = names
            .iter()
            .map(|n| {
                let svc = &cfg.services[*n];
                vec![
                    n.to_string(),
                    svc.group.clone(),
                    svc.display_name.clone(),
                    svc.unit.clone(),
                ]
            })
            .collect();
        println!(
            "{}",
            render_table(&["NAME", "GROUP", "DISPLAY", "UNIT"], &table_rows)
        );
    }
    Ok(())
}

/// Pure: whether a `toggle` of a service in `state` stops or starts it.
///
/// The running set matches the one in `members_to_stop`: `Loading` counts
/// as running (it occupies VRAM on its way in). `Activating` is *not*
/// running, so toggling an activating target takes the start path (where
/// `systemctl start` is effectively a no-op while the unit activates).
fn toggle_is_stop(state: &ServiceState) -> bool {
    matches!(
        state,
        ServiceState::Active | ServiceState::Unhealthy | ServiceState::Loading
    )
}

/// Pure: whether an `activate` for `target` is a no-op.
///
/// Only an already-`Active` target (systemd-active *and* healthy) is a no-op.
/// `Loading`/`Activating` targets must stay re-activatable so a stuck load
/// (port never opens) can be retried rather than silently no-op'd. A missing
/// target is also not a no-op (activation proceeds).
fn activate_is_noop(target: Option<&ServiceState>) -> bool {
    matches!(target, Some(ServiceState::Active))
}

/// Pure: which configured services must be stopped before `target` can start.
///
/// Given `(name, group, combined_state)` for every configured service,
/// returns the names of the *other* services in the same group as `target`
/// that are currently running (`Active`, `Unhealthy`, `Loading`, or
/// `Activating`), sorted by name. `Loading` and `Activating` count because
/// they are on their way to occupying VRAM — exclusive groups must never
/// have two models loading at once. The target itself is never included,
/// and an unknown target yields an empty vec.
fn members_to_stop(target: &str, services: &[(String, String, ServiceState)]) -> Vec<String> {
    let Some((_, target_group, _)) = services.iter().find(|(name, _, _)| name.as_str() == target)
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = services
        .iter()
        .filter(|(name, group, state)| {
            name.as_str() != target
                && group == target_group
                && matches!(
                    state,
                    ServiceState::Active
                        | ServiceState::Unhealthy
                        | ServiceState::Loading
                        | ServiceState::Activating
                )
        })
        .map(|(name, _, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// Pure: combine per-service `(display_name, combined_state, healthy)` — in
/// sorted service-name order — into the top-level Waybar fields
/// `(text, tooltip, class)` for the aggregate status view.
///
/// - text: the per-service text of the first Active AND healthy service,
///   else the first Loading service (model mid-load isn't "LLM off"),
///   else `"󰐇 LLM off"`.
/// - tooltip: `"<display_name>: <state>"` per service, joined by newlines.
/// - class: any Failed/Unhealthy → "unhealthy"; else any
///   Loading/Activating/Deactivating → "loading"; else any Active →
///   "active"; else "inactive".
fn aggregate_waybar(entries: &[(String, ServiceState, bool)]) -> (String, String, String) {
    let tooltip = entries
        .iter()
        .map(|(display, state, _)| format!("{}: {}", display, state_to_string(state)))
        .collect::<Vec<_>>()
        .join("\n");

    let text = entries
        .iter()
        .find(|(_, state, healthy)| matches!(state, ServiceState::Active) && *healthy)
        .map(|(display, _, _)| format!("{} {}", state_icon(&ServiceState::Active), display))
        .or_else(|| {
            entries
                .iter()
                .find(|(_, state, _)| matches!(state, ServiceState::Loading))
                .map(|(display, _, _)| {
                    format!("{} {}", state_icon(&ServiceState::Loading), display)
                })
        })
        .unwrap_or_else(|| "󰐇 LLM off".to_string());

    let class = if entries
        .iter()
        .any(|(_, s, _)| matches!(s, ServiceState::Failed | ServiceState::Unhealthy))
    {
        "unhealthy"
    } else if entries.iter().any(|(_, s, _)| {
        matches!(
            s,
            ServiceState::Loading | ServiceState::Activating | ServiceState::Deactivating
        )
    }) {
        "loading"
    } else if entries
        .iter()
        .any(|(_, s, _)| matches!(s, ServiceState::Active))
    {
        "active"
    } else {
        "inactive"
    };

    (text, tooltip, class.to_string())
}

/// Extract the port from a health URL like `http://127.0.0.1:8081/health`.
/// Returns `None` when the URL carries no parseable port.
fn port_from_url(url: &str) -> Option<u16> {
    let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

/// Table cell for the PORT column: the parsed port, or "-" if unparseable.
fn port_cell(url: &str) -> String {
    port_from_url(url)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// "a, b" for a non-empty list, "none" otherwise (error messages).
fn stopped_or_none(stopped: &[String]) -> String {
    if stopped.is_empty() {
        "none".to_string()
    } else {
        stopped.join(", ")
    }
}

/// Print an ActionOutput: JSON when --json, else the given human-readable
/// line.
fn print_action(out: &ActionOutput, cli: &Cli, text: &str) -> Result<(), String> {
    if cli.json {
        let json = serde_json::to_string(out).map_err(|e| e.to_string())?;
        println!("{}", json);
        Ok(())
    } else {
        println!("{}", text);
        Ok(())
    }
}

/// Emit the JSON failure/timeout record (when --json) so scripts can still
/// see which services were stopped, and return the error message.
fn emit_activate_error(
    cli: &Cli,
    service: &str,
    status: &str,
    stopped: &[String],
    message: String,
) -> String {
    if cli.json {
        let out = ActionOutput {
            service: service.to_string(),
            action: "activate".to_string(),
            status: status.to_string(),
            stopped: stopped.to_vec(),
        };
        let json = serde_json::to_string(&out);
        if let Ok(json) = json {
            println!("{}", json);
        }
    }
    message
}

/// Pure: render a fixed-width, two-space-separated column table (no trailing
/// newline).
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let fmt_row = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:width$}", cell, width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let mut lines: Vec<String> = Vec::with_capacity(rows.len() + 1);
    lines.push(fmt_row(
        &headers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    ));
    for row in rows {
        lines.push(fmt_row(row));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch base for the wiring tests' temp dirs.
    ///
    /// Deliberately under the project's `target/` dir, not the system temp
    /// dir: in some sandboxed environments the system temp dir is cleaned or
    /// otherwise interfered with while tests are running, which can delete a
    /// lock file out from under a held `flock` and break the tests.
    fn test_scratch_base() -> std::path::PathBuf {
        let base = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-scratch");
        std::fs::create_dir_all(&base).expect("create test-scratch dir");
        base
    }

    #[test]
    fn test_state_to_string() {
        assert_eq!(state_to_string(&ServiceState::Active), "active");
        assert_eq!(state_to_string(&ServiceState::Inactive), "inactive");
        assert_eq!(state_to_string(&ServiceState::Unhealthy), "unhealthy");
        assert_eq!(state_to_string(&ServiceState::Loading), "loading");
    }

    #[test]
    fn test_format_status_output_schema() {
        let output = format_status_output("test-svc", "Test Service", &ServiceState::Active, true);
        assert_eq!(output.service, "test-svc");
        assert_eq!(output.state, "active");
        assert!(output.healthy);
        assert!(!output.text.is_empty());
        assert!(!output.tooltip.is_empty());
        assert_eq!(output.class, "active");
        // Ensure serializable
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("service").is_some());
        assert!(v.get("state").is_some());
        assert!(v.get("healthy").is_some());
        assert!(v.get("text").is_some());
        assert!(v.get("tooltip").is_some());
        assert!(v.get("class").is_some());
    }

    #[test]
    fn test_format_status_output_unhealthy() {
        let output = format_status_output("svc", "Svc", &ServiceState::Unhealthy, false);
        assert_eq!(output.state, "unhealthy");
        assert!(!output.healthy);
        assert!(output.tooltip.contains("unhealthy"));
    }

    #[test]
    fn test_cli_parsing_status() {
        let args = Cli::parse_from(&["prog", "status", "--service", "llm-server", "--json"]);
        assert!(args.json);
        assert_eq!(args.service, Some("llm-server".to_string()));
        assert!(matches!(args.command, Commands::Status { .. }));
    }

    #[test]
    fn test_cli_conflicts_json_plain() {
        // clap should reject both flags
        let result =
            Cli::try_parse_from(&["prog", "status", "--service", "svc", "--json", "--plain"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_service_name_ok() {
        assert!(validate_service_name("llm-server.service").is_ok());
        assert!(validate_service_name("my_service-1@instance").is_ok());
        assert!(validate_service_name("svc").is_ok());
    }

    #[test]
    fn test_validate_service_name_bad() {
        assert!(validate_service_name("bad unit!").is_err());
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("svc space").is_err());
    }

    #[test]
    fn test_state_icon_unhealthy_distinct() {
        assert_eq!(state_icon(&ServiceState::Unhealthy), "⚠️");
        assert_ne!(
            state_icon(&ServiceState::Unhealthy),
            state_icon(&ServiceState::Active)
        );
        // Loading shares the in-progress spin icon with Activating/Deactivating.
        assert_eq!(
            state_icon(&ServiceState::Loading),
            state_icon(&ServiceState::Activating)
        );
    }

    #[test]
    fn test_format_status_output_tooltip_no_dup() {
        let output = format_status_output("svc", "Svc", &ServiceState::Unhealthy, false);
        assert!(!output.tooltip.contains("(unhealthy)"));
        assert!(output.tooltip.contains("unhealthy"));
    }

    #[test]
    fn test_action_output_serialization() {
        let out = ActionOutput {
            service: "svc".into(),
            action: "start".into(),
            status: "ok".into(),
            stopped: vec![],
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["service"], "svc");
        assert_eq!(v["action"], "start");
        assert_eq!(v["status"], "ok");
        // Empty stopped list is omitted to preserve the v1 JSON shape.
        assert!(v.get("stopped").is_none());
    }

    #[test]
    fn test_action_output_stopped_field() {
        let out = ActionOutput {
            service: "qwen".into(),
            action: "activate".into(),
            status: "ok".into(),
            stopped: vec!["muse".into()],
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["stopped"][0], "muse");
    }

    #[test]
    fn test_services_output_schema() {
        let out = ServicesOutput {
            services: vec!["a".into(), "b".into()],
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("services").is_some());
        assert_eq!(v["services"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_cli_parsing_start_stop_toggle() {
        let args = Cli::parse_from(&["prog", "start", "--service", "svc"]);
        assert!(matches!(args.command, Commands::Start { .. }));
        let args = Cli::parse_from(&["prog", "stop", "--service", "svc", "--plain"]);
        assert!(matches!(args.command, Commands::Stop { .. }));
        assert!(args.plain);
        let args = Cli::parse_from(&["prog", "toggle", "--service", "svc"]);
        assert!(matches!(args.command, Commands::Toggle));
    }

    #[test]
    fn test_waybar_json_output_format() {
        // Simulate Waybar JSON output for active service
        let output = format_status_output("llm-server", "LLM Server", &ServiceState::Active, true);
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Waybar requires text, tooltip, class fields
        assert!(v.get("text").is_some(), "Waybar JSON must contain 'text'");
        assert!(
            v.get("tooltip").is_some(),
            "Waybar JSON must contain 'tooltip'"
        );
        assert!(v.get("class").is_some(), "Waybar JSON must contain 'class'");

        let text = v["text"].as_str().unwrap();
        let tooltip = v["tooltip"].as_str().unwrap();
        let class = v["class"].as_str().unwrap();

        assert!(!text.is_empty());
        assert!(text.contains("LLM Server") || text.contains("󰍲"));
        assert!(tooltip.contains("LLM Server"));
        assert_eq!(class, "active");

        // Test inactive state
        let output_inactive =
            format_status_output("llm-server", "LLM Server", &ServiceState::Inactive, false);
        let json_inactive = serde_json::to_string(&output_inactive).unwrap();
        let v_inactive: serde_json::Value = serde_json::from_str(&json_inactive).unwrap();
        assert_eq!(v_inactive["class"], "inactive");

        // Test unhealthy state
        let output_unhealthy =
            format_status_output("llm-server", "LLM Server", &ServiceState::Unhealthy, false);
        let json_unhealthy = serde_json::to_string(&output_unhealthy).unwrap();
        let v_unhealthy: serde_json::Value = serde_json::from_str(&json_unhealthy).unwrap();
        assert_eq!(v_unhealthy["class"], "unhealthy");

        // Ensure class values are valid for CSS
        let valid_classes = [
            "active",
            "inactive",
            "unhealthy",
            "loading",
            "activating",
            "deactivating",
            "failed",
            "unknown",
        ];
        for state in [
            ServiceState::Active,
            ServiceState::Inactive,
            ServiceState::Activating,
            ServiceState::Deactivating,
            ServiceState::Failed,
            ServiceState::Unknown,
            ServiceState::Unhealthy,
            ServiceState::Loading,
        ] {
            let out = format_status_output("svc", "Svc", &state, true);
            let cls = out.class;
            assert!(
                valid_classes.contains(&cls.as_str()),
                "class '{}' is not valid for Waybar",
                cls
            );
        }
    }

    // --- v2: members_to_stop -------------------------------------------------

    #[test]
    fn test_toggle_is_stop_running_states() {
        // Running set → toggle stops.
        for state in [
            ServiceState::Active,
            ServiceState::Unhealthy,
            ServiceState::Loading,
        ] {
            assert!(toggle_is_stop(&state), "{:?} counts as running", state);
        }
        // Everything else → toggle starts.
        for state in [
            ServiceState::Inactive,
            ServiceState::Activating,
            ServiceState::Deactivating,
            ServiceState::Failed,
            ServiceState::Unknown,
        ] {
            assert!(!toggle_is_stop(&state), "{:?} counts as stopped", state);
        }
    }

    #[test]
    fn test_activate_is_noop_active_only() {
        // Active is the only no-op state.
        assert!(activate_is_noop(Some(&ServiceState::Active)));
        // Missing target is not a no-op (activation proceeds).
        assert!(!activate_is_noop(None));
        // Every other state (incl. Loading/Activating) stays re-activatable.
        for state in [
            ServiceState::Inactive,
            ServiceState::Activating,
            ServiceState::Deactivating,
            ServiceState::Failed,
            ServiceState::Unknown,
            ServiceState::Unhealthy,
            ServiceState::Loading,
        ] {
            assert!(
                !activate_is_noop(Some(&state)),
                "{:?} must be re-activatable",
                state
            );
        }
    }

    #[test]
    fn test_members_to_stop_same_group_only() {
        let services = vec![
            (
                "target".to_string(),
                "g1".to_string(),
                ServiceState::Inactive,
            ),
            ("a".to_string(), "g1".to_string(), ServiceState::Active),
            ("b".to_string(), "g2".to_string(), ServiceState::Active),
        ];
        assert_eq!(members_to_stop("target", &services), vec!["a".to_string()]);
    }

    #[test]
    fn test_members_to_stop_excludes_target() {
        let services = vec![
            ("a".to_string(), "g".to_string(), ServiceState::Active),
            ("b".to_string(), "g".to_string(), ServiceState::Unhealthy),
        ];
        assert_eq!(members_to_stop("a", &services), vec!["b".to_string()]);
    }

    #[test]
    fn test_members_to_stop_only_running_states() {
        // Inactive/Failed/Deactivating/Unknown are never stopped;
        // Active/Unhealthy/Loading/Activating are (VRAM safety: exclusive
        // groups must never have two models loading at once).
        let services = vec![
            (
                "target".to_string(),
                "g".to_string(),
                ServiceState::Inactive,
            ),
            ("a".to_string(), "g".to_string(), ServiceState::Inactive),
            ("b".to_string(), "g".to_string(), ServiceState::Failed),
            ("d".to_string(), "g".to_string(), ServiceState::Deactivating),
            ("e".to_string(), "g".to_string(), ServiceState::Unknown),
            ("f".to_string(), "g".to_string(), ServiceState::Active),
            ("h".to_string(), "g".to_string(), ServiceState::Unhealthy),
            ("i".to_string(), "g".to_string(), ServiceState::Loading),
            ("j".to_string(), "g".to_string(), ServiceState::Activating),
        ];
        assert_eq!(
            members_to_stop("target", &services),
            vec![
                "f".to_string(),
                "h".to_string(),
                "i".to_string(),
                "j".to_string()
            ]
        );
    }

    #[test]
    fn test_members_to_stop_sorted() {
        let services = vec![
            ("t".to_string(), "g".to_string(), ServiceState::Inactive),
            ("z".to_string(), "g".to_string(), ServiceState::Active),
            ("a".to_string(), "g".to_string(), ServiceState::Unhealthy),
            ("m".to_string(), "g".to_string(), ServiceState::Active),
        ];
        assert_eq!(
            members_to_stop("t", &services),
            vec!["a".to_string(), "m".to_string(), "z".to_string()]
        );
    }

    #[test]
    fn test_members_to_stop_empty_input() {
        assert!(members_to_stop("x", &[]).is_empty());
    }

    #[test]
    fn test_members_to_stop_target_not_in_list() {
        let services = vec![("a".to_string(), "g".to_string(), ServiceState::Active)];
        assert!(members_to_stop("nope", &services).is_empty());
    }

    // --- v2: aggregate status formatting -------------------------------------

    #[test]
    fn test_aggregate_class_priority() {
        // unhealthy beats active, loading, everything
        let rows = vec![
            ("A".to_string(), ServiceState::Active, true),
            ("B".to_string(), ServiceState::Failed, false),
            ("C".to_string(), ServiceState::Activating, false),
        ];
        assert_eq!(aggregate_waybar(&rows).2, "unhealthy");
        // HTTP-unhealthy also counts as unhealthy
        let rows = vec![("A".to_string(), ServiceState::Unhealthy, false)];
        assert_eq!(aggregate_waybar(&rows).2, "unhealthy");
        // loading beats active and inactive
        let rows = vec![
            ("A".to_string(), ServiceState::Inactive, false),
            ("B".to_string(), ServiceState::Active, true),
            ("C".to_string(), ServiceState::Deactivating, false),
        ];
        assert_eq!(aggregate_waybar(&rows).2, "loading");
        // active beats inactive
        let rows = vec![
            ("A".to_string(), ServiceState::Inactive, false),
            ("B".to_string(), ServiceState::Active, true),
        ];
        assert_eq!(aggregate_waybar(&rows).2, "active");
        // nothing running -> inactive
        let rows = vec![("A".to_string(), ServiceState::Inactive, false)];
        assert_eq!(aggregate_waybar(&rows).2, "inactive");
        // empty -> inactive
        assert_eq!(aggregate_waybar(&[]).2, "inactive");
    }

    #[test]
    fn test_aggregate_text_first_active_healthy() {
        let rows = vec![
            ("Off Model".to_string(), ServiceState::Inactive, false),
            ("First Active".to_string(), ServiceState::Active, true),
            ("Second Active".to_string(), ServiceState::Active, true),
        ];
        assert_eq!(
            aggregate_waybar(&rows).0,
            format!("{} First Active", state_icon(&ServiceState::Active))
        );
    }

    #[test]
    fn test_aggregate_text_fallback_off() {
        let rows = vec![
            ("A".to_string(), ServiceState::Inactive, false),
            ("B".to_string(), ServiceState::Failed, false),
        ];
        assert_eq!(aggregate_waybar(&rows).0, "󰐇 LLM off");
        assert_eq!(aggregate_waybar(&[]).0, "󰐇 LLM off");
    }

    #[test]
    fn test_aggregate_loading_class_and_text() {
        // A Loading service yields the "loading" class (no Failed/Unhealthy
        // present) and shows the loading service in `text`, not "LLM off".
        let rows = vec![
            ("Loading Model".to_string(), ServiceState::Loading, false),
            ("Off".to_string(), ServiceState::Inactive, false),
        ];
        let (text, _, class) = aggregate_waybar(&rows);
        assert_eq!(class, "loading");
        assert_eq!(
            text,
            format!("{} Loading Model", state_icon(&ServiceState::Loading))
        );
        assert!(!text.contains("LLM off"));
        // An Active healthy service still beats a Loading one in `text`.
        let rows = vec![
            ("Loading Model".to_string(), ServiceState::Loading, false),
            ("Serving".to_string(), ServiceState::Active, true),
        ];
        assert_eq!(
            aggregate_waybar(&rows).0,
            format!("{} Serving", state_icon(&ServiceState::Active))
        );
    }

    #[test]
    fn test_aggregate_tooltip_lines() {
        let rows = vec![
            ("Muse Glimmer".to_string(), ServiceState::Active, true),
            ("Qwen 3.8".to_string(), ServiceState::Inactive, false),
        ];
        assert_eq!(
            aggregate_waybar(&rows).1,
            "Muse Glimmer: active\nQwen 3.8: inactive"
        );
    }

    #[test]
    fn test_aggregate_status_output_schema() {
        let svc = format_status_output("muse", "Muse Glimmer", &ServiceState::Active, true);
        let out = AggregateStatusOutput {
            text: "󰍲 Muse Glimmer".to_string(),
            tooltip: "Muse Glimmer: active".to_string(),
            class: "active".to_string(),
            services: vec![svc],
        };
        let json = serde_json::to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("text").is_some());
        assert!(v.get("tooltip").is_some());
        assert!(v.get("class").is_some());
        assert_eq!(v["services"].as_array().unwrap().len(), 1);
        assert_eq!(v["services"][0]["service"], "muse");
    }

    #[test]
    fn test_port_from_url() {
        assert_eq!(port_from_url("http://127.0.0.1:8080/health"), Some(8080));
        assert_eq!(port_from_url("https://10.0.0.5:9091"), Some(9091));
        assert_eq!(port_from_url("http://[::1]:8081/health"), Some(8081));
        assert_eq!(port_from_url("http://127.0.0.1/health"), None);
        assert_eq!(port_from_url("not a url"), None);
    }

    #[test]
    fn test_port_cell_fallback() {
        assert_eq!(
            port_cell("http://127.0.0.1:8080/health"),
            "8080".to_string()
        );
        assert_eq!(port_cell("bogus"), "-".to_string());
        assert_eq!(port_cell("http://127.0.0.1/health"), "-".to_string());
    }

    // --- v2: clap parsing ----------------------------------------------------

    #[test]
    fn test_cli_parsing_stop_all() {
        let args = Cli::parse_from(&["prog", "stop", "--all"]);
        assert!(matches!(args.command, Commands::Stop { all: true }));
        assert!(args.service.is_none());
    }

    #[test]
    fn test_cli_parsing_status_all_json() {
        let args = Cli::parse_from(&["prog", "status", "--all", "--json"]);
        assert!(matches!(args.command, Commands::Status { all: true }));
        assert!(args.json);
        assert!(args.service.is_none());
    }

    #[test]
    fn test_cli_parsing_activate_flags() {
        let args = Cli::parse_from(&[
            "prog",
            "activate",
            "--service",
            "x",
            "--no-wait",
            "--timeout",
            "60",
        ]);
        assert!(matches!(
            args.command,
            Commands::Activate {
                no_wait: true,
                timeout: 60
            }
        ));
        assert_eq!(args.service.as_deref(), Some("x"));
    }

    #[test]
    fn test_cli_parsing_start_wait() {
        let args = Cli::parse_from(&[
            "prog",
            "start",
            "--service",
            "y",
            "--wait",
            "--timeout",
            "5",
        ]);
        assert!(matches!(
            args.command,
            Commands::Start {
                wait: true,
                timeout: 5
            }
        ));
        assert_eq!(args.service.as_deref(), Some("y"));
        // Default timeout is 300 s
        let args = Cli::parse_from(&["prog", "start", "--service", "y"]);
        assert!(matches!(
            args.command,
            Commands::Start {
                wait: false,
                timeout: 300
            }
        ));
    }

    #[test]
    fn test_cli_parsing_run() {
        let args = Cli::parse_from(&["prog", "run", "muse"]);
        assert!(matches!(
            args.command,
            Commands::Run { setup } if setup == "muse"
        ));
    }

    #[test]
    fn test_activate_fails_when_lock_held() {
        // The lock path is injected via a thread-local override (the handler
        // runs synchronously on this thread), never via the process
        // environment: mutating env from a test thread races with the other
        // test threads reading it.
        //
        // The handler loads the ambient config (user config or the repo
        // default, both of which define "muse") before acquiring the lock,
        // so no config fixture is needed.
        let dir = tempfile::tempdir_in(&test_scratch_base()).expect("tempdir");
        let lock_path = dir.path().join("llm-server-ctl.lock");
        crate::lock::set_test_lock_path(Some(lock_path.clone()));
        let held = crate::lock::ProcessLock::acquire_at(&lock_path).expect("test holds the lock");

        let cli = Cli::parse_from(["llm-server-ctl", "activate", "--service", "muse"]);
        let result = handle_activate(&cli, true, 5);

        crate::lock::set_test_lock_path(None);
        drop(held);

        let err = result.expect_err("handle_activate must fail while the lock is held");
        assert!(
            err.contains("already running"),
            "unexpected error message: {}",
            err
        );
    }

    #[test]
    fn test_stop_all_fails_when_lock_held() {
        // See `test_activate_fails_when_lock_held` for why the lock path is
        // injected via thread-local override rather than the environment.
        let dir = tempfile::tempdir_in(&test_scratch_base()).expect("tempdir");
        let lock_path = dir.path().join("llm-server-ctl.lock");
        crate::lock::set_test_lock_path(Some(lock_path.clone()));
        let held = crate::lock::ProcessLock::acquire_at(&lock_path).expect("test holds the lock");

        let cli = Cli::parse_from(["llm-server-ctl", "stop", "--all"]);
        let result = handle_stop_all(&cli);

        crate::lock::set_test_lock_path(None);
        drop(held);

        let err = result.expect_err("handle_stop_all must fail while the lock is held");
        assert!(
            err.contains("already running"),
            "unexpected error message: {}",
            err
        );
    }
}
