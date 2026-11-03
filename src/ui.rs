//! UI module using egui
//! Provides a GUI window with service list, status, controls, and logs.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use egui::{CentralPanel, Color32, Context, RichText};
use once_cell::sync::Lazy;
use std::time::Duration;

use crate::config::{load_config, ServiceConfig};
use crate::lock::ProcessLock;
use crate::service::{ServiceController, ServiceState};

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
}

impl App {
    fn load() -> Result<Self, String> {
        let cfg = load_config().map_err(|e| format!("Failed to load config: {}", e))?;
        let mut services = Vec::new();
        for (name, svc_cfg) in cfg.services {
            let controller = ServiceController::new(svc_cfg.clone(), None);
            let (state, healthy) = controller.combined_state();
            services.push(ServiceInfo {
                name,
                config: svc_cfg,
                controller,
                state,
                healthy,
                loading: false,
            });
        }
        Ok(Self {
            services,
            last_poll: Instant::now(),
            confirm_action: None,
            pending_action: None,
            selected_log_service: None,
            last_error: None,
        })
    }

    fn poll(&mut self) {
        for svc in &mut self.services {
            let prev_state = svc.state.clone();
            let (s, h) = svc.controller.combined_state();
            svc.state = s;
            svc.healthy = h;

            if prev_state != svc.state {
                let title = "Service state changed";
                let body = format!("{} is now {:?}", svc.config.display_name, svc.state);
                svc.controller.notify(title, &body);
            }
        }
        self.last_poll = Instant::now();
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

        CentralPanel::default().show(ctx, |ui| {
            ui.heading("LLM Server Indicator");
            if let Some(err) = &self.last_error {
                ui.colored_label(Color32::from_rgb(255, 0, 0), format!("Error: {}", err));
                ui.separator();
            }
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for svc in &mut self.services {
                    let color = state_color(&svc.state, svc.healthy);
                    let label = state_label(&svc.state);
                    let name = svc.name.clone();
                    let display_name = svc.config.display_name.clone();
                    let unit = svc.config.unit.clone();
                    let loading = svc.loading;
                    let healthy = svc.healthy;
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(color, RichText::new(label).strong());
                            ui.label(&display_name);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if loading {
                                        ui.spinner();
                                    }
                                    if ui.button("Logs").clicked() {
                                        // defer log fetch to next frame
                                        self.selected_log_service = Some(name.clone());
                                    }
                                },
                            );
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Start").clicked() {
                                self.confirm_action = Some(ConfirmAction::Start(name.clone()));
                            }
                            if ui.button("Stop").clicked() {
                                self.confirm_action = Some(ConfirmAction::Stop(name.clone()));
                            }
                            if ui.button("Toggle").clicked() {
                                self.confirm_action = Some(ConfirmAction::Toggle(name.clone()));
                            }
                            ui.label(format!("Unit: {}", unit));
                        });
                        ui.label(format!("Health: {}", if healthy { "OK" } else { "FAIL" }));
                    });
                    ui.add_space(8.0);
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

        // Log window
        if let Some(name) = self.selected_log_service.clone() {
            // Ensure logs are being fetched (non-blocking)
            if let Some(svc) = self.services.iter().find(|s| s.name == name) {
                // Trigger async fetch if not cached
                let unit = svc.config.unit.clone();
                let cache = logs_cache_lock();
                let cached = cache.get(&unit).cloned();
                drop(cache);
                if cached.is_none() {
                    // Spawn fetch in background
                    let unit_clone = unit.clone();
                    std::thread::spawn(move || {
                        let logs = fetch_logs_static(&unit_clone);
                        let mut cache = logs_cache_lock();
                        cache.insert(unit_clone, logs);
                    });
                }
                let mut log_text = cached.unwrap_or_else(|| "Loading logs...".to_string());
                let mut open = true;
                egui::Window::new(format!("Logs for {}", name))
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.text_edit_multiline(&mut log_text);
                        if ui.button("Refresh").clicked() {
                            // Invalidate cache and refetch
                            {
                                let mut cache = logs_cache_lock();
                                cache.remove(&unit);
                            }
                            // Trigger new fetch
                            let unit_clone = unit.clone();
                            std::thread::spawn(move || {
                                let logs = fetch_logs_static(&unit_clone);
                                let mut cache = logs_cache_lock();
                                cache.insert(unit_clone, logs);
                            });
                            log_text = "Loading logs...".to_string();
                        }
                    });
                if !open {
                    self.selected_log_service = None;
                }
            }
        }
    }
}

/// Runs the egui window via `eframe`, which owns the event loop, the
/// OpenGL context, and the paint pass. The old hand-rolled loop created an
/// `egui::Context` and fed it input but never rendered a frame, so the
/// window never actually became visible.
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
            .with_inner_size([720.0, 560.0]),
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
