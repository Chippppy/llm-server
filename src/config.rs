//! Configuration loading module

use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn default_group() -> String {
    "default".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub unit: String,
    pub display_name: String,
    pub health_url: String,
    /// Services in the same group are swapped exclusively by `activate`.
    #[serde(default = "default_group")]
    pub group: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for (key, svc) in &self.services {
            validate_service_config(svc).map_err(|e| format!("service '{}': {}", key, e))?;
            if health_url_needs_warning(&svc.health_url) {
                eprintln!(
                    "WARNING: health_url for service '{}' is neither a loopback host nor HTTPS ({}). It is polled repeatedly with no auth, so a config taken from an untrusted source could use it as a data-exfiltration beacon. Use a loopback host (127.0.0.1/::1/localhost) or an https:// URL.",
                    key, svc.health_url
                );
            }
        }
        Ok(())
    }
}

/// Extract the host part of a URL (without userinfo or port), lowercased.
/// Returns `None` if the input has no `scheme://host` shape.
fn url_host(url: &str) -> Option<String> {
    let authority = url
        .trim()
        .split_once("://")?
        .1
        .split(['/', '?', '#'])
        .next()?;
    let host_part = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if let Some(inner) = host_part.strip_prefix('[') {
        // IPv6 literal: `[addr]` or `[addr]:port`
        return inner.split(']').next().map(|h| h.to_ascii_lowercase());
    }
    let host = host_part
        .rsplit_once(':')
        .map_or(host_part, |(host, _)| host);
    Some(host.to_ascii_lowercase())
}

/// True if `health_url` points at a loopback literal (127.0.0.1, ::1, localhost).
pub fn health_url_is_loopback(url: &str) -> bool {
    matches!(
        url_host(url).as_deref(),
        Some("127.0.0.1" | "::1" | "localhost")
    )
}

/// True if `health_url` deserves a security warning: it is neither a
/// loopback host nor HTTPS — a plaintext remote endpoint polled every few
/// seconds (a config from an untrusted dotfiles repo could use it as an
/// exfiltration beacon). Remote https URLs are legitimate and not flagged.
pub fn health_url_needs_warning(url: &str) -> bool {
    !url.trim().starts_with("https://") && !health_url_is_loopback(url)
}

fn validate_service_config(cfg: &ServiceConfig) -> Result<(), String> {
    if cfg.unit.trim().is_empty() {
        return Err("unit is empty".into());
    }
    // A leading '-' would parse as a systemctl option in the argv.
    if cfg.unit.starts_with('-') {
        return Err(format!(
            "unit '{}' must not start with '-' (it would be parsed as a systemctl option)",
            cfg.unit
        ));
    }
    if cfg.group.trim().is_empty() {
        return Err("group must not be empty".into());
    }
    // regex ^[a-zA-Z0-9._@-]+$
    if !cfg
        .unit
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
    {
        return Err(format!("unit '{}' contains invalid characters", cfg.unit));
    }
    let url = cfg.health_url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "health_url '{}' does not use http/https scheme",
            cfg.health_url
        ));
    }
    // Basic URL sanity check
    if !url.contains("://") || url.len() < 8 {
        return Err(format!(
            "health_url '{}' is not a valid URL",
            cfg.health_url
        ));
    }
    Ok(())
}

pub fn load_config() -> Result<Config, String> {
    // Use dirs::config_dir() to avoid literal "~"
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
    let user_config_path = config_dir.join("llm-server-indicator/config.toml");
    // Resolve default config relative to CARGO_MANIFEST_DIR, not cwd
    let default_config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/default.toml");

    if user_config_path.exists() {
        // A user config that exists but fails to parse or validate is a
        // HARD error: silently falling back to the bundled default.toml
        // would let mutating commands act on the default service set.
        let content = fs::read_to_string(&user_config_path).map_err(|e| {
            format!(
                "Failed to read user config {}: {}",
                user_config_path.display(),
                e
            )
        })?;
        let cfg: Config = toml::from_str(&content).map_err(|e| {
            format!(
                "User config {} failed to parse: {} — fix or remove the file; not falling back to the bundled default",
                user_config_path.display(),
                e
            )
        })?;
        cfg.validate().map_err(|e| {
            format!(
                "User config {} failed validation: {} — fix or remove the file; not falling back to the bundled default",
                user_config_path.display(),
                e
            )
        })?;
        return Ok(cfg);
    }

    // No user config: fall back to the bundled default.
    eprintln!(
        "No user config at {}; falling back to {}",
        user_config_path.display(),
        default_config_path.display()
    );
    let content = fs::read_to_string(&default_config_path).map_err(|e| {
        format!(
            "Failed to read default config {}: {}",
            default_config_path.display(),
            e
        )
    })?;
    let cfg: Config = toml::from_str(&content).map_err(|e| {
        format!(
            "Failed to parse default config {}: {}",
            default_config_path.display(),
            e
        )
    })?;
    cfg.validate().map_err(|e| {
        format!(
            "Default config {} failed validation: {}",
            default_config_path.display(),
            e
        )
    })?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvSetGuard;

    fn make_cfg(unit: &str, display: &str, url: &str) -> ServiceConfig {
        ServiceConfig {
            unit: unit.into(),
            display_name: display.into(),
            health_url: url.into(),
            group: "default".into(),
        }
    }

    #[test]
    fn test_validate_service_config_ok() {
        let cfg = make_cfg(
            "llm-server@muse.service",
            "LLM",
            "http://127.0.0.1:8080/health",
        );
        assert!(validate_service_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_service_config_bad_unit() {
        let cfg = make_cfg("bad unit!", "", "http://example.com");
        assert!(validate_service_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_service_config_leading_dash_unit() {
        let cfg = make_cfg("-evil.service", "", "http://127.0.0.1:1/health");
        let err = validate_service_config(&cfg).unwrap_err();
        assert!(
            err.contains("systemctl option"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_validate_service_config_bad_url() {
        let cfg = make_cfg("good.service", "", "ftp://example.com");
        assert!(validate_service_config(&cfg).is_err());
    }

    #[test]
    fn test_validate_service_config_empty_group() {
        let mut cfg = make_cfg("good.service", "", "http://example.com/health");
        cfg.group = "   ".into();
        assert!(validate_service_config(&cfg).is_err());
    }

    #[test]
    fn test_health_url_is_loopback() {
        assert!(health_url_is_loopback("http://127.0.0.1:8080/health"));
        assert!(health_url_is_loopback("http://[::1]:8080/health"));
        assert!(health_url_is_loopback("http://localhost:8080/health"));
        assert!(!health_url_is_loopback("http://example.com/health"));
        assert!(!health_url_is_loopback("http://192.168.1.10:8080/health"));
        assert!(!health_url_is_loopback("http://user@example.com/health"));
    }

    #[test]
    fn test_health_url_needs_warning() {
        // Plaintext remote endpoint: warn (exfil-beacon risk).
        assert!(health_url_needs_warning("http://example.com/health"));
        // Remote over HTTPS: legitimate, no warning.
        assert!(!health_url_needs_warning("https://remote.example/health"));
        // Loopback hosts: no warning, even over plain http.
        assert!(!health_url_needs_warning("http://127.0.0.1:8080/health"));
        assert!(!health_url_needs_warning("http://[::1]:8080/health"));
        assert!(!health_url_needs_warning("http://localhost:8080/health"));
    }

    #[test]
    fn test_group_defaults_to_default() {
        // Omitted group should deserialize as "default"
        let toml_str = r#"
[services.x]
unit = "x.service"
display_name = "X"
health_url = "http://127.0.0.1:1/health"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.services.get("x").unwrap().group, "default");
    }

    #[test]
    fn test_load_config_fallback() {
        let _mutex = crate::test_support::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Point XDG_CONFIG_HOME at an empty dir so no user config exists:
        // load_config must fall back to the bundled default.
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = EnvSetGuard::set("XDG_CONFIG_HOME", dir.path());
        let cfg = load_config();
        assert!(cfg.is_ok(), "load_config should succeed with default");
        let cfg = cfg.unwrap();
        assert!(
            !cfg.services.is_empty(),
            "default config should contain services"
        );
    }

    #[test]
    fn test_load_config_user_config_exists_but_invalid_is_hard_error() {
        let _mutex = crate::test_support::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_dir = dir.path().join("llm-server-indicator");
        fs::create_dir_all(&cfg_dir).expect("mkdir");
        fs::write(cfg_dir.join("config.toml"), "not [valid toml").expect("write");
        let _xdg = EnvSetGuard::set("XDG_CONFIG_HOME", dir.path());
        // An existing-but-broken user config must NOT silently fall back to
        // the bundled default (mutating commands would act on the default
        // service set) — it must hard-error and point at the file.
        let err = load_config().expect_err("invalid user config must hard-error");
        assert!(
            err.contains("config.toml"),
            "error should point at the file: {}",
            err
        );
        assert!(
            err.contains("not falling back"),
            "error should state there is no fallback: {}",
            err
        );
    }

    #[test]
    fn test_load_config_user_config_exists_but_fails_validation_is_hard_error() {
        let _mutex = crate::test_support::ENV_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_dir = dir.path().join("llm-server-indicator");
        fs::create_dir_all(&cfg_dir).expect("mkdir");
        // Parses fine, but the unit contains invalid characters.
        fs::write(
            cfg_dir.join("config.toml"),
            r#"[services.x]
unit = "bad unit!"
display_name = "X"
health_url = "http://127.0.0.1:1/health"
"#,
        )
        .expect("write");
        let _xdg = EnvSetGuard::set("XDG_CONFIG_HOME", dir.path());
        let err = load_config().expect_err("invalid user config must hard-error");
        assert!(
            err.contains("config.toml"),
            "error should point at the file: {}",
            err
        );
    }
}
