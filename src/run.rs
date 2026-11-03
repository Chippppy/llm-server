//! Setup runner: `llm-server-ctl run <setup>`
//!
//! Invoked by the `llm-server@.service` systemd template unit (one instance
//! per setup). Loads `~/.config/llm-server-indicator/setups/<setup>.toml`,
//! validates it, and `exec()`s the llama.cpp server binary so that systemd
//! signals (SIGTERM on stop) go straight to the server process.

use serde::Deserialize;
use std::fs;
use std::net::{TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;

fn default_host() -> String {
    "127.0.0.1".into()
}

#[derive(Debug, Deserialize, serde::Serialize, PartialEq)]
pub struct ServerSpec {
    /// Absolute path to the llama-server binary
    pub binary: String,
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, serde::Serialize, PartialEq)]
pub struct ModelSpec {
    /// Main model GGUF
    pub main: String,
    /// Optional speculative-decoding draft GGUF
    #[serde(default)]
    pub draft: Option<String>,
    /// Spec-decoding type, required together with `draft` (e.g. "draft-dflash")
    #[serde(default)]
    pub spec_type: Option<String>,
    /// Max speculative tokens, only meaningful together with `draft`
    #[serde(default)]
    pub spec_draft_n_max: Option<u32>,
}

#[derive(Debug, Deserialize, serde::Serialize, PartialEq, Default)]
pub struct FlagsSpec {
    #[serde(default)]
    pub ngl: Option<u32>,
    #[serde(default)]
    pub ngld: Option<u32>,
    #[serde(default)]
    pub flash_attention: Option<bool>,
    #[serde(default)]
    pub ctx_size: Option<u32>,
    #[serde(default)]
    pub jinja: Option<bool>,
    #[serde(default)]
    pub temp: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub cache_type_k: Option<String>,
    #[serde(default)]
    pub cache_type_v: Option<String>,
    /// Anything llama-server takes that the schema doesn't cover
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Deserialize, serde::Serialize, PartialEq)]
pub struct SetupSpec {
    pub server: ServerSpec,
    pub model: ModelSpec,
    #[serde(default)]
    pub flags: FlagsSpec,
}

/// Directory holding the per-setup TOML files.
///
/// `LLM_SERVER_INDICATOR_SETUPS_DIR` overrides the default location (used by
/// tests and for unusual layouts).
pub fn setups_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LLM_SERVER_INDICATOR_SETUPS_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("llm-server-indicator")
        .join("setups")
}

fn validate_setup_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('.')
        || name.starts_with('-')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('@')
    {
        return Err(format!(
            "Invalid setup name '{}': must be a simple file stem \
             (no slashes, no leading dot or dash, no '@')",
            name
        ));
    }
    if name.ends_with(".template") {
        let base = name.trim_end_matches(".template");
        return Err(format!(
            "Setup '{}' is a bundled template file, not a runnable setup. \
             Copy it to ~/.config/llm-server-indicator/setups/{}.toml and fill in the real paths.",
            name, base
        ));
    }
    Ok(())
}

/// Load and validate `setups/<name>.toml` from the default setups dir.
pub fn load_setup(name: &str) -> Result<SetupSpec, String> {
    load_setup_from(&setups_dir(), name)
}

/// Load and validate `<dir>/<name>.toml`.
pub fn load_setup_from(dir: &Path, name: &str) -> Result<SetupSpec, String> {
    validate_setup_name(name)?;
    let path = dir.join(format!("{}.toml", name));
    if !path.exists() {
        return Err(format!(
            "Setup file not found: {} (expected a TOML file in {})",
            path.display(),
            setups_dir().display()
        ));
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let spec: SetupSpec = toml::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    validate_spec(&spec)?;
    Ok(spec)
}

/// Validate that a spec is runnable on this machine.
pub fn validate_spec(spec: &SetupSpec) -> Result<(), String> {
    let bin = Path::new(&spec.server.binary);
    if !bin.is_file() {
        return Err(format!("binary not found: {}", bin.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(bin)
            .map_err(|e| format!("Failed to stat {}: {}", bin.display(), e))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(format!("binary is not executable: {}", bin.display()));
        }
    }
    if !Path::new(&spec.model.main).is_file() {
        return Err(format!("main model not found: {}", spec.model.main));
    }
    if let Some(draft) = &spec.model.draft {
        if !Path::new(draft).is_file() {
            return Err(format!("draft model not found: {}", draft));
        }
    }
    if spec.model.draft.is_some() && spec.model.spec_type.is_none() {
        return Err("model.draft requires model.spec_type".into());
    }
    if spec.model.spec_draft_n_max.is_some() && spec.model.draft.is_none() {
        return Err("model.spec_draft_n_max requires model.draft".into());
    }
    if spec.server.port == 0 {
        return Err("server.port must not be 0".into());
    }
    if spec.server.host.trim().is_empty() {
        return Err("server.host must not be empty".into());
    }
    Ok(())
}

/// Build the argv (everything after the binary) for llama-server.
///
/// Order mirrors the legacy start scripts; llama-server does not care about
/// flag order, but the argv is pinned by unit tests to catch drift.
pub fn build_argv(spec: &SetupSpec) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    argv.push("-m".into());
    argv.push(spec.model.main.clone());
    if let Some(draft) = &spec.model.draft {
        argv.push("-md".into());
        argv.push(draft.clone());
    }
    if let Some(spec_type) = &spec.model.spec_type {
        argv.push("--spec-type".into());
        argv.push(spec_type.clone());
    }
    if let Some(n_max) = spec.model.spec_draft_n_max {
        argv.push("--spec-draft-n-max".into());
        argv.push(n_max.to_string());
    }
    if let Some(ngl) = spec.flags.ngl {
        argv.push("-ngl".into());
        argv.push(ngl.to_string());
    }
    if let Some(ngld) = spec.flags.ngld {
        argv.push("-ngld".into());
        argv.push(ngld.to_string());
    }
    if let Some(ctx) = spec.flags.ctx_size {
        argv.push("-c".into());
        argv.push(ctx.to_string());
    }
    if let Some(fa) = spec.flags.flash_attention {
        argv.push("-fa".into());
        argv.push(if fa { "on" } else { "off" }.into());
    }
    if let Some(ck) = &spec.flags.cache_type_k {
        argv.push("--cache-type-k".into());
        argv.push(ck.clone());
    }
    if let Some(cv) = &spec.flags.cache_type_v {
        argv.push("--cache-type-v".into());
        argv.push(cv.clone());
    }
    if spec.flags.jinja == Some(true) {
        argv.push("--jinja".into());
    }
    if let Some(temp) = spec.flags.temp {
        argv.push("--temp".into());
        argv.push(temp.to_string());
    }
    if let Some(top_p) = spec.flags.top_p {
        argv.push("--top-p".into());
        argv.push(top_p.to_string());
    }
    if let Some(top_k) = spec.flags.top_k {
        argv.push("--top-k".into());
        argv.push(top_k.to_string());
    }
    argv.push("--host".into());
    argv.push(spec.server.host.clone());
    argv.push("--port".into());
    argv.push(spec.server.port.to_string());
    argv.extend(spec.flags.extra_args.iter().cloned());
    argv
}

/// Fail fast if the setup's port is already bound (e.g. a previous instance
/// is still shutting down). Reported to journalctl via stderr.
fn check_port_free(host: &str, port: u16) -> Result<(), String> {
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("failed to resolve {}: {}", host, e))?
        .collect();
    for addr in addrs {
        TcpListener::bind(addr).map_err(|e| format!("port {} is already in use ({})", port, e))?;
    }
    Ok(())
}

/// Load the setup, pre-flight check, and replace this process with the
/// llama-server binary. Only returns on failure.
pub fn run_setup(name: &str) -> Result<(), String> {
    let spec = load_setup(name)?;
    check_port_free(&spec.server.host, spec.server.port).map_err(|e| {
        format!(
            "setup '{}': {} — refusing to start a second instance",
            name, e
        )
    })?;
    let argv = build_argv(&spec);
    eprintln!(
        "[run] setup='{}' exec {} {}",
        name,
        spec.server.binary,
        argv.join(" ")
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        match Command::new(&spec.server.binary).args(&argv).exec() {
            // exec() replaces the process; on success it never returns, so
            // the only value this match can ever see is an io::Error.
            e => Err(format!(
                "Failed to exec '{}': {} — check the setup TOML and journalctl",
                spec.server.binary, e
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(&spec.server.binary)
            .args(&argv)
            .status()
            .map_err(|e| format!("Failed to start '{}': {}", spec.server.binary, e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "'{}' exited with status {}",
                spec.server.binary, status
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_from_toml(toml_str: &str) -> SetupSpec {
        toml::from_str(toml_str).expect("test TOML should parse")
    }

    #[test]
    fn test_build_argv_muse_matches_legacy_script() {
        // Must reproduce the legacy start script values exactly (same
        // flags/values; flag order is pinned here). Paths are placeholders.
        let spec = spec_from_toml(
            r#"
[server]
binary = "/path/to/llama-server"
host = "127.0.0.1"
port = 8080

[model]
main = "/path/to/models/muse-glimmer/Muse-Glimmer-30B-KQuant-17GB-Q4_K_M.gguf"
draft = "/path/to/models/muse-glimmer/dflash-Muse-Glimmer-30B-Q4_K_M.gguf"
spec_type = "draft-dflash"
spec_draft_n_max = 15

[flags]
ngl = 99
ngld = 99
flash_attention = true
ctx_size = 131072
jinja = true
temp = 1.0
top_p = 0.95
top_k = 64
extra_args = []
"#,
        );
        let argv = build_argv(&spec);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/path/to/models/muse-glimmer/Muse-Glimmer-30B-KQuant-17GB-Q4_K_M.gguf",
                "-md",
                "/path/to/models/muse-glimmer/dflash-Muse-Glimmer-30B-Q4_K_M.gguf",
                "--spec-type",
                "draft-dflash",
                "--spec-draft-n-max",
                "15",
                "-ngl",
                "99",
                "-ngld",
                "99",
                "-c",
                "131072",
                "-fa",
                "on",
                "--jinja",
                "--temp",
                "1",
                "--top-p",
                "0.95",
                "--top-k",
                "64",
                "--host",
                "127.0.0.1",
                "--port",
                "8080",
            ]
        );
    }

    #[test]
    fn test_build_argv_qwen_no_sampling_flags() {
        // Qwen has no sampling flags but has cache-type flags.
        let spec = spec_from_toml(
            r#"
[server]
binary = "/path/to/llama-server"
host = "127.0.0.1"
port = 8081

[model]
main = "/path/to/models/qwen-3.8/Qwen3.8-27B-Q6_K.gguf"
draft = "/path/to/models/qwen-3.8/Qwen3.8-27B-DFlash2-Q8_0.gguf"
spec_type = "draft-dflash"
spec_draft_n_max = 7

[flags]
ngl = 99
ngld = 99
flash_attention = true
ctx_size = 80000
jinja = true
cache_type_k = "q8_0"
cache_type_v = "q8_0"
"#,
        );
        let argv = build_argv(&spec);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/path/to/models/qwen-3.8/Qwen3.8-27B-Q6_K.gguf",
                "-md",
                "/path/to/models/qwen-3.8/Qwen3.8-27B-DFlash2-Q8_0.gguf",
                "--spec-type",
                "draft-dflash",
                "--spec-draft-n-max",
                "7",
                "-ngl",
                "99",
                "-ngld",
                "99",
                "-c",
                "80000",
                "-fa",
                "on",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
                "--jinja",
                "--host",
                "127.0.0.1",
                "--port",
                "8081",
            ]
        );
    }

    #[test]
    fn test_build_argv_no_draft_omits_spec_flags() {
        let spec = spec_from_toml(
            r#"
[server]
binary = "/bin/llama-server"
port = 8090

[model]
main = "/models/small.gguf"

[flags]
extra_args = ["--parallel", "2"]
"#,
        );
        assert_eq!(spec_from_toml(&toml::to_string(&spec).unwrap()), spec); // round-trips (sanity)
        let argv = build_argv(&spec);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/models/small.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "8090",
                "--parallel",
                "2",
            ]
        );
        assert!(!argv.iter().any(|a| a == "-md"));
        assert!(!argv.iter().any(|a| a == "--spec-type"));
    }

    #[test]
    fn test_validate_spec_port_zero() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("x.gguf");
        fs::write(&model, b"").unwrap();
        let spec = spec_from_toml(&format!(
            r#"
[server]
binary = "/bin/true"
host = "127.0.0.1"
port = 0

[model]
main = "{}"
"#,
            model.display()
        ));
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.contains("port"), "unexpected error: {}", err);
    }

    #[test]
    fn test_validate_spec_missing_main_model() {
        let spec = spec_from_toml(
            r#"
[server]
binary = "/bin/true"
host = "127.0.0.1"
port = 8090

[model]
main = "/nonexistent/model.gguf"
"#,
        );
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.contains("main model"), "unexpected error: {}", err);
    }

    #[test]
    fn test_validate_spec_missing_binary() {
        let spec = spec_from_toml(
            r#"
[server]
binary = "/nonexistent/llama-server"
host = "127.0.0.1"
port = 8090

[model]
main = "/nonexistent/model.gguf"
"#,
        );
        let err = validate_spec(&spec).unwrap_err();
        assert!(err.contains("binary"), "unexpected error: {}", err);
    }

    /// Build a spec whose binary/model files exist, so cross-field rules
    /// (draft ⇒ spec_type, spec_draft_n_max ⇒ draft) are what gets exercised.
    fn spec_with_real_files(
        dir: &std::path::Path,
    ) -> (SetupSpec, std::path::PathBuf, std::path::PathBuf) {
        let main = dir.join("main.gguf");
        let draft = dir.join("draft.gguf");
        fs::write(&main, b"x").unwrap();
        fs::write(&draft, b"x").unwrap();
        let spec = spec_from_toml(
            r#"
[server]
binary = "/bin/true"
host = "127.0.0.1"
port = 8090

[model]
main = "REPLACED_MAIN"
draft = "REPLACED_DRAFT"
"#,
        );
        let mut spec = spec;
        spec.model.main = main.to_string_lossy().into();
        spec.model.draft = Some(draft.to_string_lossy().into());
        (spec, main, draft)
    }

    #[test]
    fn test_validate_spec_draft_requires_spec_type() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut spec, main, draft) = spec_with_real_files(tmp.path());
        let err = validate_spec(&spec).unwrap_err();
        assert!(
            err.contains("spec_type"),
            "expected draft-requires-spec_type error, got: {}",
            err
        );
        // With spec_type the rule is satisfied.
        spec.model.spec_type = Some("draft-dflash".into());
        assert!(validate_spec(&spec).is_ok());
        let _ = fs::remove_file(&main);
        let _ = fs::remove_file(&draft);
    }

    #[test]
    fn test_validate_spec_spec_n_max_requires_draft() {
        let tmp = std::env::temp_dir();
        let main = tmp.join("llm-indicator-test-main2.gguf");
        fs::write(&main, b"x").unwrap();
        let spec = spec_from_toml(
            r#"
[server]
binary = "/bin/true"
host = "127.0.0.1"
port = 8090

[model]
main = "REPLACED"
spec_draft_n_max = 5
"#,
        );
        let mut spec = spec;
        spec.model.main = main.to_string_lossy().into();
        let err = validate_spec(&spec).unwrap_err();
        assert!(
            err.contains("spec_draft_n_max"),
            "unexpected error: {}",
            err
        );
        let _ = fs::remove_file(&main);
    }

    #[test]
    fn test_validate_setup_name() {
        assert!(validate_setup_name("muse").is_ok());
        assert!(validate_setup_name("qwen-3.8").is_ok());
        assert!(validate_setup_name("").is_err());
        assert!(validate_setup_name("../etc/passwd").is_err());
        assert!(validate_setup_name("a/b").is_err());
        assert!(validate_setup_name(".hidden").is_err());
        assert!(validate_setup_name("muse.template").is_err());
        assert!(validate_setup_name("-flag").is_err());
        assert!(validate_setup_name("a@b").is_err());
    }

    #[test]
    fn test_load_setup_rejects_template_name() {
        // Even a fully valid spec file must be rejected when its name ends
        // in `.template` (bundled starting points are not runnable setups).
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("muse.template.toml"),
            r#"
[server]
binary = "/bin/true"
port = 8090

[model]
main = "/models/x.gguf"
"#,
        )
        .unwrap();
        let err = load_setup_from(dir.path(), "muse.template").unwrap_err();
        assert!(err.contains("template"), "unexpected error: {}", err);
        assert!(err.contains("muse.toml"), "unexpected error: {}", err);
        assert!(
            err.contains("fill in the real paths"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_load_setup_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_setup_from(dir.path(), "does-not-exist");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_load_setup_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("model.gguf");
        fs::write(&main, b"x").unwrap();
        fs::write(
            dir.path().join("test-setup.toml"),
            format!(
                r#"
[server]
binary = "/bin/true"
host = "127.0.0.1"
port = 8099

[model]
main = "{}"
"#,
                main.to_string_lossy()
            ),
        )
        .unwrap();
        let spec = load_setup_from(dir.path(), "test-setup").expect("load_setup should succeed");
        assert_eq!(spec.server.port, 8099);
        assert_eq!(
            build_argv(&spec),
            vec![
                "-m".to_string(),
                main.to_string_lossy().to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "8099".to_string(),
            ]
        );
    }

    /// `LLM_SERVER_INDICATOR_SETUPS_DIR` is process-global; serialize the
    /// tests that set it (each uses its own unique temp dir).
    static SETUPS_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Argv built from a self-contained muse spec fixture (same content as
    /// the shipped template, placeholder paths) must match the legacy
    /// script values. Loaded through the env-var setups-dir resolution.
    #[test]
    fn test_muse_toml_argv() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("muse.toml"),
            r#"
[server]
binary = "/path/to/llama-server"
host = "127.0.0.1"
port = 8080

[model]
main = "/path/to/models/muse-glimmer/Muse-Glimmer-30B-KQuant-17GB-Q4_K_M.gguf"
draft = "/path/to/models/muse-glimmer/dflash-Muse-Glimmer-30B-Q4_K_M.gguf"
spec_type = "draft-dflash"
spec_draft_n_max = 15

[flags]
ngl = 99
ngld = 99
ctx_size = 131072
flash_attention = true
jinja = true
temp = 1.0
top_p = 0.95
top_k = 64
extra_args = []
"#,
        )
        .unwrap();
        let _guard = SETUPS_DIR_LOCK.lock().unwrap();
        std::env::set_var("LLM_SERVER_INDICATOR_SETUPS_DIR", dir.path());
        let path = setups_dir().join("muse.toml");
        assert!(
            path.starts_with(dir.path()),
            "setups_dir() must honor LLM_SERVER_INDICATOR_SETUPS_DIR"
        );
        let content = fs::read_to_string(&path).unwrap();
        let spec: SetupSpec = toml::from_str(&content).unwrap();
        let argv = build_argv(&spec);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/path/to/models/muse-glimmer/Muse-Glimmer-30B-KQuant-17GB-Q4_K_M.gguf",
                "-md",
                "/path/to/models/muse-glimmer/dflash-Muse-Glimmer-30B-Q4_K_M.gguf",
                "--spec-type",
                "draft-dflash",
                "--spec-draft-n-max",
                "15",
                "-ngl",
                "99",
                "-ngld",
                "99",
                "-c",
                "131072",
                "-fa",
                "on",
                "--jinja",
                "--temp",
                "1",
                "--top-p",
                "0.95",
                "--top-k",
                "64",
                "--host",
                "127.0.0.1",
                "--port",
                "8080",
            ]
        );
    }

    /// Same for the qwen spec (user-validated runtime flags, placeholder paths).
    #[test]
    fn test_qwen_toml_argv() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("qwen.toml"),
            r#"
[server]
binary = "/path/to/llama-server"
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
"#,
        )
        .unwrap();
        let _guard = SETUPS_DIR_LOCK.lock().unwrap();
        std::env::set_var("LLM_SERVER_INDICATOR_SETUPS_DIR", dir.path());
        let path = setups_dir().join("qwen.toml");
        assert!(
            path.starts_with(dir.path()),
            "setups_dir() must honor LLM_SERVER_INDICATOR_SETUPS_DIR"
        );
        let content = fs::read_to_string(&path).unwrap();
        let spec: SetupSpec = toml::from_str(&content).unwrap();
        let argv = build_argv(&spec);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/path/to/models/qwen-3.8/Qwen3.8-27B-Q6_K.gguf",
                "-md",
                "/path/to/models/qwen-3.8/Qwen3.8-27B-DFlash2-Q8_0.gguf",
                "--spec-type",
                "draft-dflash",
                "--spec-draft-n-max",
                "4",
                "-ngl",
                "99",
                "-ngld",
                "99",
                "-c",
                "131072",
                "-fa",
                "on",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
                "--jinja",
                "--host",
                "127.0.0.1",
                "--port",
                "8080",
                "-np",
                "1",
                "--no-reasoning-preserve",
                "--chat-template-kwargs",
                "{\"reasoning_effort\":\"medium\"}",
            ]
        );
    }
}
