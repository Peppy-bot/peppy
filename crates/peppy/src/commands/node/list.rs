use std::path::{Path, PathBuf};
use std::{env, fs};

#[allow(dead_code)]
fn open_and_parse_file<P: AsRef<Path>>(file_path: P) -> Result<i32, Box<dyn std::error::Error>> {
    let _content = fs::read_to_string(file_path)?;
    Ok(1)
}

fn get_path_relative_to_exe(relative_path: &str) -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|exe_path| exe_path.parent().map(Path::to_path_buf))
        .map(|exe_dir| exe_dir.join(relative_path))
}

pub fn list_nodes() {
    todo!();
    #[allow(unreachable_code)]
    let _peppy_config = get_path_relative_to_exe("./peppy.star");
}

pub fn check() {
    todo!();
}
