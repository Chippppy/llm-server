fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--gui") {
        if let Err(e) = llm_server_indicator::ui::run_gui() {
            eprintln!("GUI error: {}", e);
            std::process::exit(1);
        }
    } else {
        if let Err(e) = llm_server_indicator::cli::run() {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
