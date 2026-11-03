//! Integration tests for the v2 CLI contract.
//!
//! Hermeticity: `load_config()` prefers `$XDG_CONFIG_HOME/llm-server-indicator/config.toml`
//! over the repo's `config/default.toml`, and some machines have a legacy v1
//! user config that would shadow it. Every test therefore points
//! `XDG_CONFIG_HOME` at a shared temporary fixture containing a v2-style
//! config. The fixture's units do not exist and its health URLs use loopback
//! ports that are never bound, so no test can reach a real systemd unit or
//! the real LLM server. Only in-process calls are made: no test spawns the
//! `llm-server-ctl` binary (the JSON output schemas are covered by the
//! hermetic unit tests in `src/cli.rs`). `OnceLock` makes this parallel-safe:
//! all tests in this process share the same fixture.

use llm_server_indicator::config::{load_config, ServiceConfig};
use llm_server_indicator::service::{ServiceController, ServiceState};
use std::path::Path;
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
unit = "llm-server@locktest-fake.service"
display_name = "Muse Glimmer 30B"
health_url = "http://127.0.0.1:1/health"
group = "default"

[services.qwen]
unit = "llm-server@locktest-fake2.service"
display_name = "Qwen 3.8 27B"
health_url = "http://127.0.0.1:2/health"
group = "default"
"#,
            )
            .expect("failed to write fixture config");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            dir
        })
        .path()
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
    assert_eq!(muse.unit, "llm-server@locktest-fake.service");
    assert_eq!(muse.display_name, "Muse Glimmer 30B");
    assert_eq!(muse.health_url, "http://127.0.0.1:1/health");
    assert_eq!(muse.group, "default");
    let qwen = cfg.services.get("qwen").unwrap();
    assert_eq!(qwen.unit, "llm-server@locktest-fake2.service");
    assert_eq!(qwen.display_name, "Qwen 3.8 27B");
    assert_eq!(qwen.health_url, "http://127.0.0.1:2/health");
    assert_eq!(qwen.group, "default");
    cfg.validate().expect("config should be valid");
}

#[test]
fn test_service_controller_creation_and_validation() {
    let _ = fixture_xdg_config_home();
    let cfg = load_config().expect("config load");
    let svc_cfg = cfg.services.get("muse").unwrap().clone();
    let controller = ServiceController::new(svc_cfg, None);
    assert_eq!(
        controller.service_config.unit,
        "llm-server@locktest-fake.service"
    );
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
    // Health check for unreachable URL should be false. (Port 1 on loopback
    // is never bound for an unprivileged user; the connect is refused
    // kernel-locally and no data is sent.)
    let health = bad_controller.health();
    assert!(!health, "unreachable health url should be false");
}
