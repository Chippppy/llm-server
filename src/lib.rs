//! Library root for llm-server-indicator

// Public module declarations
pub mod cli;
pub mod config;
pub mod lock;
pub mod run;
pub mod service;
pub mod ui;
pub mod vram;

/// Shared helpers for tests that mutate process-wide state (env vars).
#[cfg(test)]
pub mod test_support {
    /// Process-wide mutex for tests that mutate environment variables:
    /// env vars are process-wide, so two such tests running on different
    /// threads — including in *different* modules — would clobber each
    /// other's values. Every test that sets an env var must hold this.
    pub static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: sets one env var and removes it on drop (even on panic),
    /// so a failing test never leaks a value into the other test threads.
    pub struct EnvSetGuard {
        key: &'static str,
    }

    impl EnvSetGuard {
        pub fn set(key: &'static str, value: &std::path::Path) -> Self {
            // SAFETY: test-only; the shared `ENV_MUTEX` serializes env
            // mutation across test threads.
            unsafe { std::env::set_var(key, value) };
            Self { key }
        }
    }

    impl Drop for EnvSetGuard {
        fn drop(&mut self) {
            // SAFETY: test-only; holds `ENV_MUTEX` via the owning test.
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_builds() {
        assert!(true);
    }
}
