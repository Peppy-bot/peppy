use std::process::Command;

pub fn execute_pixi(args: &[String]) {
    let pixi_path = if let Some(path) = option_env!("PIXI_BINARY_PATH") {
        path
    } else {
        eprintln!("Error: Pixi binary not found. Please rebuild with the 'build_pixi' feature enabled:");
        eprintln!("  cargo build --features build_pixi");
        eprintln!("\nAlternatively, ensure pixi is installed and available in your PATH.");
        std::process::exit(1);
    };

    let status = Command::new(pixi_path)
        .args(args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Failed to execute pixi from dependency: {}", e);
            eprintln!("Pixi binary path: {}", pixi_path);
            std::process::exit(1);
        });

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}
