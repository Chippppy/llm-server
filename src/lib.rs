//! Library root for llm-server-indicator

// Public module declarations
pub mod cli;
pub mod config;
pub mod lock;
pub mod run;
pub mod service;
pub mod tray;
pub mod ui;

pub fn startup_message() -> &'static str {
    "llm-server-indicator starting"
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_builds() {
        assert!(true);
    }
}
