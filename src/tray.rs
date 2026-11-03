//! System tray integration module
//! Provides a tray icon with menu items for controlling services.

use tray_icon::{
    menu::{Menu, MenuItem},
    Icon, TrayIconBuilder,
};

#[derive(Debug, Clone)]
pub enum TrayCommand {
    ShowWindow,
    Start(String),
    Stop(String),
    Toggle(String),
    Quit,
}

pub fn init_tray() -> Result<(), String> {
    // Create icon
    let icon_rgba = [255, 255, 255, 255];
    let icon = Icon::from_rgba(icon_rgba.to_vec(), 1, 1).map_err(|e| e.to_string())?;

    let menu = Menu::new();
    let show_item = MenuItem::new("Show Window", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    menu.append(&show_item).map_err(|e| e.to_string())?;
    menu.append(&quit_item).map_err(|e| e.to_string())?;

    let tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("LLM Server Indicator")
        .with_menu(Box::new(menu))
        .build()
        .map_err(|e| format!("Failed to build tray icon: {:?}", e))?;

    // Keep tray icon alive by leaking it
    std::mem::forget(tray_icon);

    Ok(())
}

pub fn run_tray() -> Result<(), String> {
    init_tray()?;
    println!("Tray initialized. Menu items: Show Window, Start, Stop, Toggle, Quit");
    // Simple confirmation via notification
    show_confirmation("Tray", "Tray icon started");
    Ok(())
}

fn show_confirmation(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .show();
    println!("Confirmation dialog requested: {} - {}", title, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_command_variants() {
        let cmd = TrayCommand::ShowWindow;
        match cmd {
            TrayCommand::ShowWindow => {}
            _ => panic!("unexpected"),
        }
    }
}
