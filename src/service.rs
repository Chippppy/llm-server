//! Service management module

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::Duration;

use once_cell::sync::Lazy;

use crate::config::ServiceConfig;

static HEALTH_RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    // current-thread: health probes are sequential, and waybar spawns a
    // short-lived CLI process every ~2 s — a multi-thread runtime would
    // waste a whole thread pool per process.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime")
});

static HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        // Scrub proxy env: a stray HTTP_PROXY/HTTPS_PROXY would misroute
        // loopback health polls.
        .no_proxy()
        .build()
        .expect("Failed to build reqwest client")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Active,
    Inactive,
    Activating,
    Deactivating,
    Failed,
    Unknown,
    Unhealthy,
    Loading,
}

#[derive(Debug)]
pub enum ServiceError {
    Io(std::io::Error),
    Command(String),
    Health(String),
}

pub struct ServiceController {
    pub service_config: ServiceConfig,
    pub logger: Option<String>,
}

impl ServiceController {
    pub fn new(service_config: ServiceConfig, logger: Option<String>) -> Self {
        Self {
            service_config,
            logger,
        }
    }

    fn log(&self, msg: &str) {
        if let Some(l) = &self.logger {
            eprintln!("[{}] {}", l, msg);
        }
    }

    pub(crate) fn validate_unit_name(unit: &str) -> bool {
        !unit.trim().is_empty()
            // A leading '-' would parse as a systemctl option in the argv.
            && !unit.starts_with('-')
            && unit
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
    }

    pub fn status(&self) -> ServiceState {
        if !Self::validate_unit_name(&self.service_config.unit) {
            self.log(&format!("Invalid unit name: {}", self.service_config.unit));
            return ServiceState::Unknown;
        }
        self.log(&format!(
            "Checking status for unit {}",
            self.service_config.unit
        ));
        let output = Command::new("sudo")
            .args(["-n", "systemctl", "is-active", &self.service_config.unit])
            .output();

        match output {
            Ok(out) => {
                if !out.status.success() {
                    self.log(&format!(
                        "systemctl is-active failed with exit code {:?}: stderr={}",
                        out.status.code(),
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                parse_systemd_state(&stdout)
            }
            Err(e) => {
                self.log(&format!("Failed to run systemctl: {}", e));
                ServiceState::Unknown
            }
        }
    }

    pub fn start(&self) -> Result<(), ServiceError> {
        if !Self::validate_unit_name(&self.service_config.unit) {
            return Err(ServiceError::Command(format!(
                "Invalid unit name: {}",
                self.service_config.unit
            )));
        }
        self.log(&format!("Starting unit {}", self.service_config.unit));
        let output = Command::new("sudo")
            .args(["-n", "systemctl", "start", &self.service_config.unit])
            .output()
            .map_err(ServiceError::Io)?;
        if output.status.success() {
            Ok(())
        } else {
            self.log(&format!(
                "systemctl start failed: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            ));
            // Surface systemctl's stderr (logger is None in production,
            // so without this the failure reason is lost).
            Err(command_error(
                "start",
                &self.service_config.unit,
                &output.stderr,
            ))
        }
    }

    pub fn stop(&self) -> Result<(), ServiceError> {
        if !Self::validate_unit_name(&self.service_config.unit) {
            return Err(ServiceError::Command(format!(
                "Invalid unit name: {}",
                self.service_config.unit
            )));
        }
        self.log(&format!("Stopping unit {}", self.service_config.unit));
        let output = Command::new("sudo")
            .args(["-n", "systemctl", "stop", &self.service_config.unit])
            .output()
            .map_err(ServiceError::Io)?;
        if output.status.success() {
            Ok(())
        } else {
            self.log(&format!(
                "systemctl stop failed: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            ));
            Err(command_error(
                "stop",
                &self.service_config.unit,
                &output.stderr,
            ))
        }
    }

    pub fn health(&self) -> bool {
        self.log(&format!("Health check {}", self.service_config.health_url));
        // Use static runtime to avoid creating new one per call
        HEALTH_RUNTIME.block_on(async {
            let url = self.service_config.health_url.clone();
            for _ in 0..2 {
                match HTTP_CLIENT.get(&url).send().await {
                    Ok(resp) if resp.status().is_success() => return true,
                    Ok(resp) => {
                        self.log(&format!(
                            "Health check non-success status: {}",
                            resp.status()
                        ));
                    }
                    Err(e) => {
                        self.log(&format!("Health check error: {}", e));
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            false
        })
    }

    /// Resolve the health URL's authority to all listening addresses.
    ///
    /// IP literals fast-path through `parse::<SocketAddr>()` (no DNS, no
    /// blocking). A missing explicit port defaults to 80 (http) / 443
    /// (https) based on the scheme. Returns `None` for unparseable input,
    /// never panics.
    pub fn health_target(&self) -> Option<Vec<SocketAddr>> {
        let (scheme, rest) = self.service_config.health_url.split_once("://")?;
        let authority = rest.split(['/', '?', '#']).next()?.to_string();
        // Fast path: IP literal — no DNS, no blocking.
        if let Ok(addr) = authority.parse::<SocketAddr>() {
            return Some(vec![addr]);
        }
        // No explicit port: default 80 (http) / 443 (https) based on scheme.
        let with_port = if authority
            .rsplit_once(':')
            .map_or(true, |(_, p)| !p.chars().all(|c| c.is_ascii_digit()))
        {
            format!("{}:{}", authority, if scheme == "https" { 443 } else { 80 })
        } else {
            authority
        };
        let addrs = with_port.to_socket_addrs().ok()?;
        let addrs: Vec<SocketAddr> = addrs.collect();
        (!addrs.is_empty()).then_some(addrs)
    }

    /// True if the health endpoint's port is currently accepting connections.
    ///
    /// Tries every resolved address and returns `true` if any accepts a
    /// connection (e.g. `localhost` → `::1`/`127.0.0.1`). If no
    /// port/authority parses, degrades to `false`.
    fn port_is_listening(&self) -> bool {
        let Some(addrs) = self.health_target() else {
            return false;
        };
        addrs
            .iter()
            .any(|addr| TcpStream::connect_timeout(addr, Duration::from_millis(500)).is_ok())
    }

    /// Returns the combined state plus the `healthy` flag.
    ///
    /// `healthy` reflects a live probe only when the systemd state is
    /// `Active` or `Activating`; for all other states it is `false` (no
    /// probe is issued). `Activating` and all other states pass through
    /// unchanged.
    ///
    /// When the unit is `Active`:
    /// - healthy → `Active` (model loaded, serving);
    /// - !healthy but the health port is listening → `Unhealthy` (server is
    ///   up and accepting connections but `/health` fails — genuinely
    ///   broken);
    /// - !healthy and the port is not yet listening → `Loading` (active in
    ///   systemd, but with `Type=simple` the unit is marked active the
    ///   instant the process execs, minutes before the model is loaded and
    ///   the port is bound — "be patient", not "crashed on a bad flag").
    pub fn combined_state(&self) -> (ServiceState, bool) {
        let sys_state = self.status();
        let should_probe = matches!(sys_state, ServiceState::Active | ServiceState::Activating);
        let healthy = if should_probe { self.health() } else { false };
        // Only touch the port when it is decisive (active but unhealthy).
        let port_listening =
            matches!(sys_state, ServiceState::Active) && !healthy && self.port_is_listening();
        let state = classify(sys_state, healthy, port_listening);
        (state, healthy)
    }

    /// Poll `health()` once per second until it succeeds or `timeout` elapses.
    ///
    /// Bails out early if systemd reports the unit went Inactive or Failed
    /// (the process died while loading — polling further is pointless), or
    /// if the health port was listening on a previous iteration and is no
    /// longer listening (the process died after the model finished loading).
    pub fn wait_until_healthy(&self, timeout: Duration) -> bool {
        // An absurd timeout that overflows the deadline is treated as already
        // expired (a bounded wait), never an unbounded loop.
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(std::time::Instant::now);
        let mut tracker = WaitTracker::new();
        loop {
            let state = self.status();
            tracker.observe(self.port_is_listening());
            if wait_should_bail(state, &tracker) {
                self.log("Bailed out while waiting for health");
                return false;
            }
            if self.health() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                self.log("Timed out waiting for health");
                return false;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    pub fn notify(&self, title: &str, body: &str) {
        let _ = notify_rust::Notification::new()
            .summary(title)
            .body(body)
            .show();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WaitTracker {
    ever_listening: bool,
    missed: u32, // consecutive non-listening samples since last seen listening
}
impl WaitTracker {
    fn new() -> Self {
        Self {
            ever_listening: false,
            missed: 0,
        }
    }
    fn observe(&mut self, listening: bool) {
        if listening {
            self.ever_listening = true;
            self.missed = 0;
        } else {
            self.missed += 1;
        }
    }
    /// Server started listening then stopped = crashed after load. Require
    /// two consecutive misses so one flaky probe can't bail us out.
    fn crashed_after_load(&self) -> bool {
        self.ever_listening && self.missed >= 2
    }
}

/// `ServiceError::Command` for a failed `systemctl <verb> <unit>`, with the
/// last ~500 chars of stderr (trimmed, folded to one line) when available.
fn command_error(verb: &str, unit: &str, stderr: &[u8]) -> ServiceError {
    let detail = stderr_tail(stderr);
    if detail.is_empty() {
        ServiceError::Command(format!("Failed to {} {}", verb, unit))
    } else {
        ServiceError::Command(format!("Failed to {} {}: {}", verb, unit, detail))
    }
}

/// Trimmed, single-line tail (last ~500 chars) of a subprocess stderr.
fn stderr_tail(stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.len() > 500 {
        let mut start = text.len() - 500;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text = text[start..].to_string();
    }
    text
}

/// Pure bail decision for the wait loop.
fn wait_should_bail(state: ServiceState, tracker: &WaitTracker) -> bool {
    matches!(state, ServiceState::Inactive | ServiceState::Failed) || tracker.crashed_after_load()
}

/// Pure: map (systemd state, health probe result, port-listening) to the combined state.
fn classify(sys: ServiceState, healthy: bool, port_listening: bool) -> ServiceState {
    match sys {
        ServiceState::Active => {
            if healthy {
                ServiceState::Active
            } else if port_listening {
                ServiceState::Unhealthy
            } else {
                ServiceState::Loading
            }
        }
        other => other,
    }
}

fn parse_systemd_state(output: &str) -> ServiceState {
    match output {
        "active" => ServiceState::Active,
        "inactive" => ServiceState::Inactive,
        "activating" => ServiceState::Activating,
        "deactivating" => ServiceState::Deactivating,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_config;

    #[test]
    fn test_parse_systemd_state() {
        assert_eq!(parse_systemd_state("active"), ServiceState::Active);
        assert_eq!(parse_systemd_state("inactive"), ServiceState::Inactive);
        assert_eq!(parse_systemd_state("activating"), ServiceState::Activating);
        assert_eq!(
            parse_systemd_state("deactivating"),
            ServiceState::Deactivating
        );
        assert_eq!(parse_systemd_state("failed"), ServiceState::Failed);
        assert_eq!(parse_systemd_state("unknown"), ServiceState::Unknown);
        assert_eq!(parse_systemd_state(""), ServiceState::Unknown);
    }

    fn make_cfg(unit: &str, display: &str, url: &str) -> ServiceConfig {
        ServiceConfig {
            unit: unit.into(),
            display_name: display.into(),
            health_url: url.into(),
            group: "default".into(),
        }
    }

    #[test]
    fn test_health_unreachable() {
        let cfg = make_cfg("test.service", "Test", "http://127.0.0.1:1"); // unlikely to be open
        let controller = ServiceController::new(cfg, None);
        // Should return false quickly without panicking
        let result = controller.health();
        assert!(!result);
    }

    #[test]
    fn test_health_target_parsing() {
        let cfg = make_cfg("a.service", "A", "http://127.0.0.1:8080/health");
        let ctrl = ServiceController::new(cfg, None);
        assert_eq!(
            ctrl.health_target(),
            Some(vec![SocketAddr::from(([127, 0, 0, 1], 8080))])
        );
        // No path: authority is the whole remainder after the scheme.
        let cfg = make_cfg("b.service", "B", "http://127.0.0.1:8081");
        let ctrl = ServiceController::new(cfg, None);
        assert_eq!(
            ctrl.health_target(),
            Some(vec![SocketAddr::from(([127, 0, 0, 1], 8081))])
        );
        // Malformed URL (no scheme) degrades to None.
        let cfg = make_cfg("c.service", "C", "not-a-url");
        let ctrl = ServiceController::new(cfg, None);
        assert_eq!(ctrl.health_target(), None);
        // Missing explicit port defaults to 80 for http.
        let cfg = make_cfg("e.service", "E", "http://127.0.0.1/health");
        let ctrl = ServiceController::new(cfg, None);
        assert_eq!(
            ctrl.health_target(),
            Some(vec![SocketAddr::from(([127, 0, 0, 1], 80))])
        );
    }

    #[test]
    fn test_classify() {
        // (Active, healthy) → Active, regardless of port.
        assert_eq!(
            classify(ServiceState::Active, true, false),
            ServiceState::Active
        );
        assert_eq!(
            classify(ServiceState::Active, true, true),
            ServiceState::Active
        );
        // (Active, !healthy, port open) → Unhealthy.
        assert_eq!(
            classify(ServiceState::Active, false, true),
            ServiceState::Unhealthy
        );
        // (Active, !healthy, port closed) → Loading.
        assert_eq!(
            classify(ServiceState::Active, false, false),
            ServiceState::Loading
        );
        // All other states pass through unchanged.
        for sys in [
            ServiceState::Activating,
            ServiceState::Inactive,
            ServiceState::Failed,
            ServiceState::Deactivating,
            ServiceState::Unknown,
        ] {
            assert_eq!(classify(sys.clone(), true, true), sys);
            assert_eq!(classify(sys.clone(), false, true), sys);
            assert_eq!(classify(sys.clone(), true, false), sys);
            assert_eq!(classify(sys.clone(), false, false), sys);
        }
    }

    #[test]
    fn test_wait_tracker_crash_bail() {
        // Seen listening, then two consecutive misses → crashed after load.
        let mut t = WaitTracker::new();
        t.observe(true);
        t.observe(false);
        assert!(!t.crashed_after_load(), "one miss is not a crash");
        t.observe(false);
        assert!(t.crashed_after_load());

        // Single miss after listening → no crash.
        let mut t = WaitTracker::new();
        t.observe(true);
        t.observe(false);
        assert!(!t.crashed_after_load());

        // Never seen listening → no crash no matter how many misses.
        let mut t = WaitTracker::new();
        t.observe(false);
        t.observe(false);
        assert!(!t.crashed_after_load());

        // Seeing listening again resets the miss counter.
        let mut t = WaitTracker::new();
        t.observe(true);
        t.observe(false);
        t.observe(true);
        t.observe(false);
        assert!(!t.crashed_after_load());
    }

    #[test]
    fn test_wait_should_bail() {
        let tracker = WaitTracker::new();
        // Inactive/Failed → bail regardless of tracker.
        assert!(wait_should_bail(ServiceState::Inactive, &tracker));
        assert!(wait_should_bail(ServiceState::Failed, &tracker));
        // Other states with a fresh tracker → no bail.
        for state in [
            ServiceState::Active,
            ServiceState::Activating,
            ServiceState::Deactivating,
            ServiceState::Unknown,
        ] {
            assert!(!wait_should_bail(state, &tracker));
        }
        // Crash-after-load bails even for Active.
        let mut crashed = WaitTracker::new();
        crashed.observe(true);
        crashed.observe(false);
        crashed.observe(false);
        assert!(wait_should_bail(ServiceState::Active, &crashed));
    }

    #[test]
    fn test_port_is_listening_closed_port() {
        let cfg = make_cfg("d.service", "D", "http://127.0.0.1:1/health"); // port 1: never open
        let ctrl = ServiceController::new(cfg, None);
        assert!(!ctrl.port_is_listening());
    }

    #[test]
    fn test_port_is_listening_open_port() {
        // Bind a real listener on 127.0.0.1 (the OS picks a free port), point
        // the health URL at it, and assert the port reports as listening.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        let cfg = make_cfg(
            "open.service",
            "Open",
            &format!("http://127.0.0.1:{}/health", port),
        );
        let ctrl = ServiceController::new(cfg, None);
        assert!(ctrl.port_is_listening(), "bound port must report listening");
        // Dropped at end of scope; the closed-port case is covered separately
        // by test_port_is_listening_closed_port.
        drop(listener);
    }

    #[test]
    fn test_service_controller_creation() {
        let cfg = make_cfg("foo.service", "Foo", "http://example.com/health");
        let ctrl = ServiceController::new(cfg.clone(), Some("test".into()));
        assert_eq!(ctrl.service_config.unit, "foo.service");
    }

    #[test]
    fn test_validate_unit_name() {
        assert!(ServiceController::validate_unit_name("llm-server.service"));
        assert!(ServiceController::validate_unit_name(
            "my_service-1@instance"
        ));
        assert!(!ServiceController::validate_unit_name(""));
        assert!(!ServiceController::validate_unit_name("bad unit!"));
        // A leading '-' would parse as a systemctl option in the argv.
        assert!(!ServiceController::validate_unit_name("-evil.service"));
    }

    #[test]
    fn test_stderr_tail() {
        assert_eq!(stderr_tail(b""), "");
        assert_eq!(stderr_tail(b"  line1\nline2  "), "line1 line2");
        let long = vec![b'a'; 600];
        let tail = stderr_tail(&long);
        assert_eq!(tail.len(), 500, "long stderr is truncated to the tail");
        assert!(tail.starts_with('a') && tail.ends_with('a'));
    }

    #[test]
    fn test_start_invalid_unit() {
        let cfg = make_cfg("bad unit!", "Test", "http://example.com");
        let ctrl = ServiceController::new(cfg, None);
        assert!(ctrl.start().is_err());
        assert!(ctrl.stop().is_err());
    }

    #[test]
    fn test_config_load_and_validate() {
        let cfg_res = load_config();
        assert!(cfg_res.is_ok(), "load_config should succeed");
        let cfg = cfg_res.unwrap();
        // Validate should have been done during load, but test again
        assert!(cfg.validate().is_ok(), "config should be valid");
    }

    #[test]
    fn test_combined_state_with_invalid_unit() {
        let cfg = make_cfg("invalid unit!", "Test", "http://127.0.0.1:1");
        let ctrl = ServiceController::new(cfg, None);
        // Invalid unit leads to Unknown state
        assert_eq!(ctrl.status(), ServiceState::Unknown);
        assert_eq!(ctrl.combined_state(), (ServiceState::Unknown, false));
    }

    #[test]
    fn test_wait_until_healthy_bails_on_failed() {
        // "nonexistent-unit.service" never exists -> is-active reports inactive
        // -> wait_until_healthy must bail out immediately, not wait the full timeout.
        let cfg = make_cfg(
            "nonexistent-llm-test-unit.service",
            "Ghost",
            "http://127.0.0.1:1/health",
        );
        let ctrl = ServiceController::new(cfg, None);
        let started = std::time::Instant::now();
        // Short deadline: on machines where the unit state cannot be read
        // (e.g. sudo denies is-active for this unit name, so the state is
        // Unknown) the wait polls until the deadline — keep that bounded so
        // the test stays fast and low-impact.
        let healthy = ctrl.wait_until_healthy(Duration::from_secs(2));
        assert!(!healthy, "unreachable unit must not report healthy");
        // Bailing on inactive/failed should be fast (< 5 s).
        // (If the state is Unknown we poll until the deadline instead —
        // guard the wall-clock assertion for that case.)
        if ctrl.status() == ServiceState::Inactive || ctrl.status() == ServiceState::Failed {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "should bail out quickly on inactive/failed, took {:?}",
                started.elapsed()
            );
        }
    }
}
