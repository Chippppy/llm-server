//! UI module using egui
//! Provides a GUI window with a service list, per-service context-window
//! control, start/stop controls, and an in-window log panel.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use egui::{Color32, Context, RichText, Ui};
use once_cell::sync::Lazy;

use crate::config::{load_config, ServiceConfig};
use crate::lock::ProcessLock;
use crate::run;
use crate::service::{ServiceController, ServiceState};
use crate::vram;

static LOGS_CACHE: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Lock the log cache, recovering from poisoning: a panic in a background
/// fetch thread must not take down the GUI.
fn logs_cache_lock() -> MutexGuard<'static, HashMap<String, String>> {
    LOGS_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, PartialEq)]
enum ConfirmAction {
    Start(String),
    Stop(String),
    Toggle(String),
}

struct ServiceInfo {
    name: String,
    config: ServiceConfig,
    controller: ServiceController,
    state: ServiceState,
    healthy: bool,
    loading: bool,
    /// Setup name derived from the unit (`llm-server@<name>.service`).
    /// `None` means the unit is not a template unit — no setup file, hence
    /// no context control for this service.
    setup_name: Option<String>,
    /// Currently configured `ctx_size` from the setup file.
    ctx_value: Option<u32>,
    /// VRAM-derived maximum context, refreshed on load and on state changes.
    ctx_estimate: Option<vram::CtxEstimate>,
    /// Non-fatal note about the estimate (setup file problem, fallback).
    ctx_note: Option<String>,
}

struct App {
    services: Vec<ServiceInfo>,
    last_poll: Instant,
    confirm_action: Option<ConfirmAction>,
    pending_action: Option<ConfirmAction>,
    selected_log_service: Option<String>,
    /// Last failed action's error, shown above the service list (cleared on
    /// the next successful action).
    last_error: Option<String>,
    /// Free GPU VRAM (GiB display helper) from the last estimate refresh.
    gpu_free_mib: Option<u64>,
}

impl App {
    fn load() -> Result<Self, String> {
        let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
        let mut services = Vec::new();
        for (name, svc_cfg) in cfg.services {
            let controller = ServiceController::new(svc_cfg.clone(), None);
            let (state, healthy) = controller.combined_state();
            let (setup_name, ctx_value, ctx_note) = match run::setup_name_from_unit(&svc_cfg.unit) {
                Some(setup) => match vram::load_spec_raw(&setup) {
                    Ok(spec) => (Some(setup), spec.flags.ctx_size, None),
                    Err(e) => (Some(setup), None, Some(format!("setup file: {e}"))),
                },
                None => (None, None, None),
            };
            services.push(ServiceInfo {
                name,
                config: svc_cfg,
                controller,
                state,
                healthy,
                loading: false,
                setup_name,
                ctx_value,
                ctx_estimate: None,
                ctx_note,
            });
        }
        let mut app = Self {
            services,
            last_poll: Instant::now(),
            confirm_action: None,
            pending_action: None,
            selected_log_service: None,
            last_error: None,
            gpu_free_mib: None,
        };
        app.selected_log_service = app.services.first().map(|s| s.name.clone());
        app.recompute_estimates();
        if let Some(unit) = app.selected_log_service.as_ref().and_then(|n| {
            app.services
                .iter()
                .find(|s| &s.name == n)
                .map(|s| s.config.unit.clone())
        }) {
            Self::fetch_logs_async(&unit);
        }
        Ok(app)
    }

    fn poll(&mut self) {
        let mut changed = false;
        for svc in &mut self.services {
            let prev_state = svc.state.clone();
            let (s, h) = svc.controller.combined_state();
            svc.state = s;
            svc.healthy = h;

            if prev_state != svc.state {
                changed = true;
                let title = "Service state changed";
                let body = format!("{} is now {:?}", svc.config.display_name, svc.state);
                svc.controller.notify(title, &body);
            }
        }
        // VRAM changed under us: refresh the context estimates (one
        // nvidia-smi call + GGUF header reads, only on transitions).
        if changed {
            self.recompute_estimates();
        }
        self.last_poll = Instant::now();
    }

    /// Re-read free VRAM and refresh the context estimate of every service
    /// that has a setup file.
    fn recompute_estimates(&mut self) {
        let free = vram::query_free_vram_mib().ok();
        self.gpu_free_mib = free;
        for svc in &mut self.services {
            let Some(setup) = &svc.setup_name else {
                continue;
            };
            match vram::estimate_for_setup(setup, free) {
                Ok(est) => {
                    svc.ctx_note = if est.warnings.is_empty() {
                        None
                    } else {
                        Some(est.warnings.join("; "))
                    };
                    svc.ctx_estimate = Some(est);
                }
                Err(e) => {
                    svc.ctx_estimate = None;
                    svc.ctx_note = Some(e);
                }
            }
        }
    }

    fn perform_action(&mut self, action: &ConfirmAction) -> Result<(), String> {
        let (name, act) = match action {
            ConfirmAction::Start(n) => (n.clone(), "start"),
            ConfirmAction::Stop(n) => (n.clone(), "stop"),
            ConfirmAction::Toggle(n) => (n.clone(), "toggle"),
        };
        let idx = self
            .services
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| format!("Service '{}' not found", name))?;

        // Same serialization as the CLI's `toggle`/`activate`: hold the
        // non-blocking process lock for the whole action so a concurrent
        // `llm-server-ctl` command cannot interleave. If another command
        // holds it, fail fast with the lock's "already running" message.
        let _lock = ProcessLock::acquire().map_err(|e| format!("{}: {}", act, e))?;

        // Fresh (name, group, combined_state) for every configured service —
        // the same input shape the CLI feeds to `members_to_stop`.
        let mut entries: Vec<(String, String, ServiceState)> = self
            .services
            .iter()
            .map(|s| {
                let (state, _) = s.controller.combined_state();
                (s.name.clone(), s.config.group.clone(), state)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let target_state = entries
            .iter()
            .find(|(n, _, _)| n == &name)
            .map(|(_, _, state)| state.clone());

        // `toggle` starts iff the target is not running (same rule as the
        // CLI's `toggle_is_stop`).
        let is_start = match act {
            "start" => true,
            "stop" => false,
            _ => !target_state
                .as_ref()
                .map_or(false, crate::cli::toggle_is_stop),
        };

        let res = if is_start {
            // Exclusive groups: stop same-group peers before starting, exactly
            // like the CLI's `activate`/`toggle`, so two models in one group
            // never load at once. An already-Active target is a no-op there,
            // so it skips the peer stop (matching `activate`'s no-op rule).
            let peers = if matches!(target_state, Some(ServiceState::Active)) {
                Vec::new()
            } else {
                crate::cli::members_to_stop(&name, &entries)
            };
            let mut stopped = Vec::new();
            for peer in peers {
                let Some(p) = self.services.iter_mut().find(|s| s.name == peer) else {
                    continue;
                };
                p.loading = true;
                let r = p.controller.stop();
                p.loading = false;
                if let Err(e) = r {
                    return Err(format!(
                        "Failed to stop '{}' before starting '{}': {:?} (stopped: {})",
                        peer,
                        name,
                        e,
                        stopped.join(", ")
                    ));
                }
                stopped.push(peer);
            }
            let svc = &mut self.services[idx];
            svc.loading = true;
            let r = svc.controller.start();
            svc.loading = false;
            r.map_err(|e| {
                format!(
                    "Failed to start '{}': {:?} (stopped: {})",
                    name,
                    e,
                    stopped.join(", ")
                )
            })
        } else {
            let svc = &mut self.services[idx];
            svc.loading = true;
            let r = svc.controller.stop();
            svc.loading = false;
            r.map_err(|e| format!("Failed to stop '{}': {:?}", name, e))
        };

        let svc = &mut self.services[idx];
        let (s, h) = svc.controller.combined_state();
        svc.state = s;
        svc.healthy = h;
        res
    }

    /// Fetch `journalctl` for a unit in a background thread and store the
    /// result in the log cache.
    fn fetch_logs_async(unit: &str) {
        let unit = unit.to_string();
        std::thread::spawn(move || {
            let logs = fetch_logs_static(&unit);
            logs_cache_lock().insert(unit, logs);
        });
    }
}

fn state_color(state: &ServiceState, healthy: bool) -> Color32 {
    match state {
        ServiceState::Active if healthy => Color32::from_rgb(0, 200, 0),
        ServiceState::Active => Color32::from_rgb(255, 165, 0),
        ServiceState::Inactive => Color32::from_rgb(150, 150, 150),
        ServiceState::Activating | ServiceState::Deactivating => Color32::from_rgb(100, 100, 255),
        ServiceState::Failed => Color32::from_rgb(255, 0, 0),
        ServiceState::Unhealthy => Color32::from_rgb(255, 100, 0),
        ServiceState::Unknown => Color32::from_rgb(200, 200, 200),
        ServiceState::Loading => Color32::from_rgb(0x9a, 0x6e, 0xe8),
    }
}

fn state_label(state: &ServiceState) -> &'static str {
    match state {
        ServiceState::Active => "Active",
        ServiceState::Inactive => "Inactive",
        ServiceState::Activating => "Activating",
        ServiceState::Deactivating => "Deactivating",
        ServiceState::Failed => "Failed",
        ServiceState::Unknown => "Unknown",
        ServiceState::Unhealthy => "Unhealthy",
        ServiceState::Loading => "Loading",
    }
}

/// Format a number with thousands separators (Rust's `format!` has no `,`).
fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fetch_logs_static(unit: &str) -> String {
    let output = Command::new("journalctl")
        .args(["-u", unit, "-n", "200", "--no-pager"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).to_string(),
        _ => "Failed to fetch logs".to_string(),
    }
}

impl App {
    fn ui(&mut self, ctx: &Context) {
        // Process pending action
        if let Some(action) = self.pending_action.take() {
            match self.perform_action(&action) {
                Ok(()) => self.last_error = None,
                Err(e) => self.last_error = Some(e),
            }
        }

        // Logs live in a resizable bottom panel inside the main window —
        // shown first so it reserves its space before the central panel.
        egui::TopBottomPanel::bottom("log_panel")
            .default_height(240.0)
            .min_height(120.0)
            .resizable(true)
            .show(ctx, |ui| self.log_panel(ui));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("LLM Server Indicator");
                if let Some(free) = self.gpu_free_mib {
                    ui.label(
                        RichText::new(format!("GPU free {:.1} GiB", free as f64 / 1024.0))
                            .weak()
                            .small(),
                    );
                }
            });
            if let Some(err) = &self.last_error {
                ui.colored_label(
                    Color32::from_rgb(255, 90, 90),
                    RichText::new(format!("Error: {err}")).small(),
                );
                ui.separator();
            }
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for svc in &mut self.services {
                    Self::service_card(ui, svc, &mut self.confirm_action, &mut self.last_error);
                    ui.add_space(10.0);
                }
            });
        });

        // Confirm dialog
        if let Some(action) = self.confirm_action.clone() {
            egui::Window::new("Confirm action")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    let (name, verb) = match &action {
                        ConfirmAction::Start(n) => (n, "start"),
                        ConfirmAction::Stop(n) => (n, "stop"),
                        ConfirmAction::Toggle(n) => (n, "toggle"),
                    };
                    ui.label(format!(
                        "Are you sure you want to {} service '{}'?",
                        verb, name
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            self.pending_action = Some(action.clone());
                            self.confirm_action = None;
                        }
                        if ui.button("No").clicked() {
                            self.confirm_action = None;
                        }
                    });
                });
        }
    }

    /// One service card: status, unit, context-window slider, and controls.
    fn service_card(
        ui: &mut Ui,
        svc: &mut ServiceInfo,
        confirm_action: &mut Option<ConfirmAction>,
        last_error: &mut Option<String>,
    ) {
        let color = state_color(&svc.state, svc.healthy);
        egui::Frame::group(ui.style())
            .inner_margin(8.0)
            .show(ui, |ui| {
                // Title row: status dot + label + name, health badge right-aligned
                ui.horizontal(|ui| {
                    ui.colored_label(color, RichText::new("●").strong());
                    ui.colored_label(
                        color,
                        RichText::new(state_label(&svc.state)).strong().size(15.0),
                    );
                    ui.label(RichText::new(&svc.config.display_name).size(15.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if svc.loading {
                            ui.spinner();
                        }
                        let (text, c) = if svc.healthy {
                            ("health ok", Color32::from_rgb(130, 220, 130))
                        } else {
                            ("health fail", Color32::from_rgb(255, 130, 130))
                        };
                        ui.label(RichText::new(text).small().color(c));
                    });
                });
                ui.label(
                    RichText::new(format!("unit {}", svc.config.unit))
                        .small()
                        .weak(),
                );

                // --- Context-window slider ---
                let setup = svc.setup_name.clone();
                // Locked while the model occupies (or is taking) VRAM: the value
                // only takes effect on the next start anyway.
                let locked = matches!(
                    svc.state,
                    ServiceState::Active
                        | ServiceState::Loading
                        | ServiceState::Activating
                        | ServiceState::Unhealthy
                );
                if let Some(setup_name) = setup {
                    let est = svc.ctx_estimate.clone();
                    let max = est
                        .as_ref()
                        .map(|e| e.max_ctx.max(vram::MIN_CTX))
                        .unwrap_or(vram::FALLBACK_MAX_CTX);
                    let before = svc
                        .ctx_value
                        .unwrap_or(vram::MIN_CTX)
                        .clamp(vram::MIN_CTX, max);
                    let mut val = before;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("context").strong());
                        ui.add(egui::Slider::new(&mut val, vram::MIN_CTX..=max).text("ctx"));
                        ui.label(RichText::new(thousands(val)).monospace().strong());
                        if locked {
                            ui.label(RichText::new("locked while running").weak().small());
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        match &est {
                            Some(e) if e.estimated => {
                                ui.label(
                                    RichText::new(format!(
                                    "max ≈ {}  (free {:.1} GiB − 1 GiB safety − {:.1} GiB weights)",
                                    thousands(e.max_ctx),
                                    e.free_vram_mib.unwrap_or(0) as f64 / 1024.0,
                                    e.weights_on_gpu_mib as f64 / 1024.0
                                ))
                                    .small()
                                    .weak(),
                                );
                            }
                            Some(_) => {
                                ui.label(
                                    RichText::new(format!(
                                        "max {} (generic estimate)",
                                        thousands(max)
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                            None => {
                                ui.label(
                                    RichText::new(format!("max {}", thousands(max)))
                                        .small()
                                        .weak(),
                                );
                            }
                        }
                        if let Some(note) = &svc.ctx_note {
                            ui.label(
                                RichText::new(note)
                                    .small()
                                    .color(Color32::from_rgb(255, 180, 80)),
                            );
                        }
                    });
                    if val != before && !locked {
                        match vram::update_ctx_size(&setup_name, val) {
                            Ok(()) => svc.ctx_value = Some(val),
                            Err(e) => {
                                *last_error =
                                    Some(format!("Failed to save context {}: {e}", thousands(val)))
                            }
                        }
                    }
                }

                // --- Controls ---
                ui.horizontal(|ui| {
                    let name = svc.name.clone();
                    if ui.button("Start").clicked() {
                        *confirm_action = Some(ConfirmAction::Start(name.clone()));
                    }
                    if ui.button("Stop").clicked() {
                        *confirm_action = Some(ConfirmAction::Stop(name.clone()));
                    }
                    if ui.button("Toggle").clicked() {
                        *confirm_action = Some(ConfirmAction::Toggle(name));
                    }
                });
            });
    }

    /// In-window log panel: service picker, refresh, and a monospace view of
    /// the selected service's journal.
    fn log_panel(&mut self, ui: &mut Ui) {
        let options: Vec<(String, String)> = self
            .services
            .iter()
            .map(|s| (s.name.clone(), s.config.display_name.clone()))
            .collect();
        let current = self.selected_log_service.clone();
        let current_display = options
            .iter()
            .find(|(n, _)| Some(n) == current.as_ref())
            .map(|(_, d)| d.clone());

        let mut picked: Option<String> = None;
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Logs").strong());
            if let Some(label) = current_display {
                egui::ComboBox::new("log_service", label).show_ui(ui, |ui| {
                    for (name, display) in &options {
                        if ui
                            .selectable_label(Some(name) == current.as_ref(), display)
                            .clicked()
                        {
                            picked = Some(name.clone());
                            ui.close_menu();
                        }
                    }
                });
            }
            if ui.button("Refresh").clicked() {
                refresh = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("journalctl, last 200 lines").weak().small());
            });
        });
        if let Some(name) = picked {
            self.selected_log_service = Some(name);
        }

        let unit: Option<String> = self
            .selected_log_service
            .as_ref()
            .and_then(|n| self.services.iter().find(|s| &s.name == n))
            .map(|s| s.config.unit.clone());

        if refresh {
            if let Some(u) = &unit {
                logs_cache_lock().remove(u);
                Self::fetch_logs_async(u);
            }
        }

        ui.separator();
        let text = match &unit {
            Some(u) => match logs_cache_lock().get(u).cloned() {
                Some(t) => t,
                None => {
                    Self::fetch_logs_async(u);
                    "Loading logs…".to_string()
                }
            },
            None => "No service selected".to_string(),
        };
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.label(RichText::new(text).monospace().size(11.5));
            });
    }
}

/// Runs the egui window via `eframe`, which owns the event loop, the
/// OpenGL context, and the paint pass.
impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // Poll service state at most every 2 s and keep the window alive by
        // scheduling the next repaint; user interaction repaints on its own.
        if self.last_poll.elapsed() >= Duration::from_secs(2) {
            self.poll();
        }
        self.ui(ctx);
        ctx.request_repaint_after(Duration::from_secs(2));
    }
}

pub fn run_gui() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LLM Server Indicator")
            .with_inner_size([820.0, 700.0]),
        // System theme detection is unreliable on Linux; keep the dark theme.
        follow_system_theme: false,
        default_theme: eframe::Theme::Dark,
        ..Default::default()
    };

    eframe::run_native(
        "LLM Server Indicator",
        options,
        // Config errors are surfaced in the UI (via `last_error`) rather than
        // aborting startup, so the user still sees the window.
        Box::new(|_| {
            Box::new(App::load().unwrap_or_else(|e| App {
                services: Vec::new(),
                last_poll: Instant::now(),
                confirm_action: None,
                pending_action: None,
                selected_log_service: None,
                last_error: Some(e),
                gpu_free_mib: None,
            }))
        }),
    )
    .map_err(|e| format!("Failed to run GUI: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ServiceState;

    #[test]
    fn test_state_label() {
        assert_eq!(state_label(&ServiceState::Active), "Active");
        assert_eq!(state_label(&ServiceState::Unhealthy), "Unhealthy");
        assert_eq!(state_label(&ServiceState::Loading), "Loading");
    }

    #[test]
    fn test_state_color_active_healthy() {
        let color = state_color(&ServiceState::Active, true);
        assert_eq!(color, Color32::from_rgb(0, 200, 0));
    }

    #[test]
    fn test_confirm_action_variants() {
        let a = ConfirmAction::Start("svc".into());
        match a {
            ConfirmAction::Start(n) => assert_eq!(n, "svc"),
            _ => panic!("wrong variant"),
        }
    }
}
