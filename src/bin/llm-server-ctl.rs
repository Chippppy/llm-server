fn main() {
    if let Err(e) = llm_server_indicator::cli::run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
