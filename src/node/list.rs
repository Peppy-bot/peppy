use core::num;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

// TODO move those errors to a dedicated package
enum CliError {
    IoError(io::Error),
    ParseError(num::ParseIntError),
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        CliError::IoError(error)
    }
}

impl From<num::ParseIntError> for CliError {
    fn from(error: num::ParseIntError) -> Self {
        CliError::ParseError(error)
    }
}

fn open_and_parse_file<P: AsRef<Path>>(file_path: P) -> Result<i32, CliError> {
    todo!();
    let mut content = fs::read_to_string(file_path)?;
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
    let peppy_config = get_path_relative_to_exe("./peppy.star");
}

pub fn check() {
    todo!();
}
