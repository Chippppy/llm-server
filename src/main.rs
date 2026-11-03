fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Only the first token after the program name selects the GUI; a `--gui`
    // anywhere else (e.g. in a value position) must not hijack the command.
    if args.get(1).is_some_and(|a| a == "--gui") {
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
