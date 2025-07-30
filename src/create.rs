use std::fs;
use std::path::{Path, PathBuf};
use std::io::Write;

pub fn create(node_name: &str, to_dir: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let node_path = match to_dir {
        Some(dir) => dir,
        None => std::env::current_dir()?.join(node_name),
    };
    
    fs::create_dir_all(&node_path)?;
    
    create_pixi_toml(&node_path)?;
    create_peppy_star(&node_path)?;
    
    println!("Created node '{}' at: {}", node_name, node_path.display());
    
    Ok(())
}

fn create_pixi_toml(node_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pixi_toml_path = node_path.join("pixi.toml");
    let mut file = fs::File::create(pixi_toml_path)?;
    
    let pixi_content = r#"[project]
name = "peppy-node"
version = "0.1.0"
description = "A peppy node"
authors = ["peppy-user"]
channels = ["conda-forge"]
platforms = ["linux-64", "osx-64", "osx-arm64", "win-64"]

[dependencies]

[tasks]
"#;
    
    file.write_all(pixi_content.as_bytes())?;
    
    Ok(())
}

fn create_peppy_star(node_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let peppy_star_path = node_path.join("peppy.star");
    let mut file = fs::File::create(peppy_star_path)?;
    
    let peppy_content = r#"# Peppy configuration file
# Define your node configuration here

def main():
    print("Hello from peppy!")
"#;
    
    file.write_all(peppy_content.as_bytes())?;
    
    Ok(())
}