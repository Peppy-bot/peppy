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
    Pixi {
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
            let mut cmd = Command::new("pixi");
            cmd.args(&args);
            
            match cmd.status() {
                Ok(status) => {
                    if !status.success() {
                        std::process::exit(status.code().unwrap_or(1));
                    }
                }
                Err(e) => {
                    eprintln!("Failed to execute pixi: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
