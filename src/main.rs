use clap::{Parser, Subcommand};
use std::process::Command;

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "A simple CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(short, long)]
        name: String,
    },
    /// Run pixi commands (e.g. peppy pixi install, peppy pixi list)
    Pixi {
        /// Arguments to pass to the pixi CLI
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Run { name } => {
            println!("Running with name: {}", name);
        }
        Commands::Pixi { args } => {
            let status = Command::new("pixi")
                .args(&args)
                .status()
                .unwrap_or_else(|e| {
                    eprintln!("Failed to execute pixi: {}", e);
                    std::process::exit(1);
                });
            
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
}
