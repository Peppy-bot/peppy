//! Materialize a hardware generation's embedded collision meshes to a directory, so a
//! non-Rust consumer (the Python sim initializer) can obtain them from this single source
//! at build/deploy time instead of baking a copy into its container image.
//!
//! Usage: `emit_meshes <v1|v2> <dir>`

use std::path::Path;
use std::process::ExitCode;

use openarm_description::HardwareVersion;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(version), Some(dir), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: emit_meshes <v1|v2> <dir>");
        return ExitCode::from(2);
    };

    let version = match version.parse::<HardwareVersion>() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    match version.write_meshes_to(Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("emit_meshes: writing {version} meshes to {dir}: {e}");
            ExitCode::FAILURE
        }
    }
}
