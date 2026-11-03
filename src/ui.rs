//! UI module using egui
//! Provides a GUI window with service list, status, controls, logs and tray integration.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use egui::{CentralPanel, Color32, Context, RichText};
use egui_winit::State;
use once_cell::sync::Lazy;
use winit::event::{Event, WindowEvent};
use winit::event_loop::EventLoopBuilder;
use winit::window::WindowBuilder;

use crate::config::{load_config, ServiceConfig};
use crate::service::{ServiceController, ServiceState};

static LOGS_CACHE: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

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
    last_state: Option<ServiceState>,
    loading: bool,
}

struct App {
    services: Vec<ServiceInfo>,
    last_poll: Instant,
    confirm_action: Option<ConfirmAction>,
    pending_action: Option<ConfirmAction>,
    selected_log_service: Option<String>,
}

impl App {
    fn new() -> Result<Self, String> {
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
                last_state: None,
                loading: false,
            });
        }
        Ok(Self {
            services,
            last_poll: Instant::now(),
            confirm_action: None,
            pending_action: None,
            selected_log_service: None,
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
                svc.last_state = Some(svc.state.clone());
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
        if let Some(svc) = self.services.iter_mut().find(|s| s.name == name) {
            svc.loading = true;
            let res = match act {
                "start" => svc.controller.start(),
                "stop" => svc.controller.stop(),
                "toggle" => {
                    let is_running = matches!(
                        svc.state,
                        ServiceState::Active | ServiceState::Unhealthy | ServiceState::Loading
                    );
                    if is_running {
                        svc.controller.stop()
                    } else {
                        svc.controller.start()
                    }
                }
                _ => Ok(()),
            };
            svc.loading = false;
            let (s, h) = svc.controller.combined_state();
            svc.state = s;
            svc.healthy = h;
            res.map_err(|e| format!("Action failed: {:?}", e))
        } else {
            Err(format!("Service {} not found", name))
        }
    }

    // fetch_logs removed, use static function
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
            let _ = self.perform_action(&action);
        }

        CentralPanel::default().show(ctx, |ui| {
            ui.heading("LLM Server Indicator");
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
                let cache = LOGS_CACHE.lock().unwrap();
                let cached = cache.get(&unit).cloned();
                drop(cache);
                if cached.is_none() {
                    // Spawn fetch in background
                    let unit_clone = unit.clone();
                    std::thread::spawn(move || {
                        let logs = fetch_logs_static(&unit_clone);
                        let mut cache = LOGS_CACHE.lock().unwrap();
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
                                let mut cache = LOGS_CACHE.lock().unwrap();
                                cache.remove(&unit);
                            }
                            // Trigger new fetch
                            let unit_clone = unit.clone();
                            std::thread::spawn(move || {
                                let logs = fetch_logs_static(&unit_clone);
                                let mut cache = LOGS_CACHE.lock().unwrap();
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

pub fn run_gui() -> Result<(), String> {
    let event_loop = EventLoopBuilder::<()>::with_user_event()
        .build()
        .map_err(|e| format!("Failed to create event loop: {}", e))?;
    let window = WindowBuilder::new()
        .with_title("LLM Server Indicator")
        .build(&event_loop)
        .map_err(|e| format!("Failed to create window: {}", e))?;
    let egui_ctx = egui::Context::default();
    let mut egui_state = State::new(
        egui_ctx.clone(),
        egui::ViewportId::ROOT,
        &window,
        Some(window.scale_factor() as f32),
        None,
    );
    let mut app = App::new()?;
    let mut last_poll = Instant::now();

    let _ = event_loop.run(move |event, _| match event {
        Event::WindowEvent { event, .. } => {
            let response = egui_state.on_window_event(&window, &event);
            if !response.consumed {
                match event {
                    WindowEvent::CloseRequested => {
                        std::process::exit(0);
                    }
                    WindowEvent::RedrawRequested => {
                        if last_poll.elapsed() >= std::time::Duration::from_secs(2) {
                            app.poll();
                            last_poll = Instant::now();
                        }
                        let raw_input = egui_state.take_egui_input(&window);
                        let output = egui_ctx.run(raw_input, |ctx| {
                            app.ui(ctx);
                        });
                        egui_state.handle_platform_output(&window, output.platform_output);
                        window.request_redraw();
                    }
                    _ => {}
                }
            }
        }
        Event::NewEvents(_) => {
            window.request_redraw();
        }
        _ => {}
    });
    Ok(())
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
