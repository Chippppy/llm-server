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
        }
        Ok(())
    }
}

fn validate_service_config(cfg: &ServiceConfig) -> Result<(), String> {
    if cfg.unit.trim().is_empty() {
        return Err("unit is empty".into());
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

    let candidates = [user_config_path, default_config_path];
    let mut last_error = None;

    for path in &candidates {
        if !path.exists() {
            eprintln!("Config path does not exist: {}", path.display());
            continue;
        }
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(cfg) => match cfg.validate() {
                    Ok(_) => return Ok(cfg),
                    Err(e) => {
                        eprintln!("Config validation failed for {}: {}", path.display(), e);
                        last_error = Some(e);
                        continue;
                    }
                },
                Err(e) => {
                    eprintln!("Failed to parse TOML from {}: {}", path.display(), e);
                    last_error = Some(format!("parse error: {}", e));
                    continue;
                }
            },
            Err(e) => {
                eprintln!("Failed to read config {}: {}", path.display(), e);
                last_error = Some(format!("read error: {}", e));
                continue;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| "No valid config found".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Should succeed via default config
        let cfg = load_config();
        assert!(cfg.is_ok(), "load_config should succeed with default");
        let cfg = cfg.unwrap();
        assert!(
            !cfg.services.is_empty(),
            "default config should contain services"
        );
    }
}
