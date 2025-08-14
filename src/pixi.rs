use std::process::Command;

pub fn execute_pixi(args: &[String]) {
    let pixi_path = option_env!("PIXI_BINARY_PATH")
        .expect("PIXI_BINARY_PATH not set. The build_pixi feature should be enabled by default.");

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
