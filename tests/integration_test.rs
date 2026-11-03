//! Integration tests for the v2 CLI contract.
//!
//! Hermeticity: `load_config()` prefers `$XDG_CONFIG_HOME/llm-server-indicator/config.toml`
//! over the repo's `config/default.toml`, and some machines have a legacy v1
//! user config that would shadow it. Every test — in-process or via a spawned
//! binary — therefore points `XDG_CONFIG_HOME` at a shared temporary fixture
//! containing a v2-style config. `OnceLock` makes this parallel-safe: all
//! tests in this process share the same fixture.

use llm_server_indicator::config::{load_config, ServiceConfig};
use llm_server_indicator::service::{ServiceController, ServiceState};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use tempfile::TempDir;

static FIXTURE: OnceLock<TempDir> = OnceLock::new();

/// Create (once) a temp XDG_CONFIG_HOME holding a v2-style config and point
/// the process env at it. All tests share this fixture.
fn fixture_xdg_config_home() -> &'static Path {
    FIXTURE
        .get_or_init(|| {
            let dir = TempDir::new().expect("failed to create temp fixture dir");
            let config_dir = dir.path().join("llm-server-indicator");
            std::fs::create_dir_all(&config_dir).expect("failed to create fixture config dir");
            std::fs::write(
                config_dir.join("config.toml"),
                r#"[services.muse]
unit = "llm-server@muse.service"
display_name = "Muse Glimmer 30B"
health_url = "http://127.0.0.1:8080/health"
group = "default"

[services.qwen]
unit = "llm-server@qwen.service"
display_name = "Qwen 3.8 27B"
health_url = "http://127.0.0.1:8081/health"
group = "default"
"#,
            )
            .expect("failed to write fixture config");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            dir
        })
        .path()
}

/// Run the `llm-server-ctl` binary against the fixture config and parse its
/// stdout as JSON. Prefers the already-built binary
/// (`CARGO_BIN_EXE_llm-server-ctl`, set by cargo test); falls back to
/// `cargo run` if that variable is not set.
fn run_ctl_json(args: &[&str]) -> Value {
    let fixture = fixture_xdg_config_home();
    let mut cmd = match std::env::var("CARGO_BIN_EXE_llm-server-ctl") {
        Ok(path) => Command::new(path),
        Err(_) => {
            let mut cmd = Command::new("cargo");
            cmd.args([
                "run",
                "--quiet",
                "--offline",
                "--bin",
                "llm-server-ctl",
                "--",
            ]);
            cmd.current_dir(env!("CARGO_MANIFEST_DIR"));
            cmd
        }
    };
    cmd.args(args).env("XDG_CONFIG_HOME", fixture);
    let output = cmd.output().expect("failed to execute llm-server-ctl");
    assert!(
        output.status.success(),
        "llm-server-ctl {:?} should succeed; stdout: {}; stderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "llm-server-ctl {:?} should emit valid JSON ({}); stdout: {}",
            args, e, stdout
        )
    })
}

#[test]
fn test_config_load_end_to_end() {
    let _ = fixture_xdg_config_home();
    let cfg = load_config().expect("config should load from fixture");
    assert!(!cfg.services.is_empty(), "config should contain services");
    assert!(
        cfg.services.contains_key("muse"),
        "should contain muse service"
    );
    assert!(
        cfg.services.contains_key("qwen"),
        "should contain qwen service"
    );
    let muse = cfg.services.get("muse").unwrap();
    assert_eq!(muse.unit, "llm-server@muse.service");
    assert_eq!(muse.display_name, "Muse Glimmer 30B");
    assert_eq!(muse.health_url, "http://127.0.0.1:8080/health");
    assert_eq!(muse.group, "default");
    let qwen = cfg.services.get("qwen").unwrap();
    assert_eq!(qwen.unit, "llm-server@qwen.service");
    assert_eq!(qwen.display_name, "Qwen 3.8 27B");
    assert_eq!(qwen.health_url, "http://127.0.0.1:8081/health");
    assert_eq!(qwen.group, "default");
    cfg.validate().expect("config should be valid");
}

#[test]
fn test_service_controller_creation_and_validation() {
    let _ = fixture_xdg_config_home();
    let cfg = load_config().expect("config load");
    let svc_cfg = cfg.services.get("muse").unwrap().clone();
    let controller = ServiceController::new(svc_cfg, None);
    assert_eq!(controller.service_config.unit, "llm-server@muse.service");
    // Invalid unit should lead to Unknown state without requiring systemctl
    let bad_cfg = ServiceConfig {
        unit: "bad unit!".into(),
        display_name: "Bad".into(),
        health_url: "http://127.0.0.1:1".into(),
        group: "default".into(),
    };
    let bad_controller = ServiceController::new(bad_cfg, None);
    assert_eq!(bad_controller.status(), ServiceState::Unknown);
    let (state, healthy) = bad_controller.combined_state();
    assert_eq!(state, ServiceState::Unknown);
    assert!(!healthy);
    // Health check for unreachable URL should be false
    let health = bad_controller.health();
    assert!(!health, "unreachable health url should be false");
}

#[test]
fn test_cli_json_output_services() {
    let json = run_ctl_json(&["services", "--json"]);
    let services = json
        .get("services")
        .and_then(Value::as_array)
        .expect("JSON should contain a services array");
    let names: Vec<&str> = services.iter().filter_map(Value::as_str).collect();
    assert!(names.contains(&"muse"), "services should include muse");
    assert!(names.contains(&"qwen"), "services should include qwen");
}

#[test]
fn test_cli_json_output_status_schema() {
    // Single-service status always exits 0 with JSON: without systemd the
    // unit simply resolves to the Unknown state.
    let json = run_ctl_json(&["status", "--service", "muse", "--json"]);
    assert_eq!(
        json.get("service").and_then(Value::as_str),
        Some("muse"),
        "JSON should identify the queried service"
    );
    for key in ["state", "healthy", "text", "tooltip", "class"] {
        assert!(
            json.get(key).is_some(),
            "JSON should contain {} for Waybar",
            key
        );
    }
}

#[test]
fn test_cli_json_output_aggregate_status_schema() {
    // The aggregate status is what the Waybar module renders directly.
    let json = run_ctl_json(&["status", "--all", "--json"]);
    for key in ["text", "tooltip", "class"] {
        assert!(
            json.get(key).is_some(),
            "aggregate JSON should contain top-level {}",
            key
        );
    }
    let services = json
        .get("services")
        .and_then(Value::as_array)
        .expect("aggregate JSON should contain a services array");
    assert_eq!(
        services.len(),
        2,
        "aggregate should report both configured services"
    );
    let names: Vec<&str> = services
        .iter()
        .filter_map(|row| row.get("service").and_then(Value::as_str))
        .collect();
    assert!(
        names.contains(&"muse"),
        "aggregate services should include muse"
    );
    assert!(
        names.contains(&"qwen"),
        "aggregate services should include qwen"
    );
    for row in services {
        for key in ["state", "healthy", "text", "tooltip", "class"] {
            assert!(row.get(key).is_some(), "service row should contain {}", key);
        }
    }
}
