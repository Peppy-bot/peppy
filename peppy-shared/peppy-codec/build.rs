//! Embeds the Cap'n Proto compiler of the build target, with the feature
//! `embedded-compiler`, and compiles the fixture schemas of the tests.
//!
//! The binary is the one that `build-helpers` locates in the tree
//! (`peppy-config-model/tools`), the binary of the target, not of the build
//! host: it runs where the crate runs.
//!
//! The fixture schemas under `tests/fixtures` become typed Cap'n Proto
//! accessors, the "generated codec" that the typed round-trip test holds the
//! run-time codec against. The schemas are the renderer's output for the
//! fixture formats, as `tests/schema_golden.rs` pins. A copy of the crate
//! that peppy deploys into a node has no `tests` directory, and compiles no
//! fixture.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const FIXTURES_DIR: &str = "tests/fixtures";
const FIXTURES: &[&str] = &["everything", "frame"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    if env::var_os("CARGO_FEATURE_EMBEDDED_COMPILER").is_some() {
        embed_compiler(&out_dir);
    }
    if Path::new(FIXTURES_DIR).is_dir() {
        compile_fixtures(&out_dir);
    }
}

fn embed_compiler(out_dir: &Path) {
    let target = build_helpers::build_target_triple();
    let binary_path = build_helpers::bundled_capnp_for_embedding(&target)
        .unwrap_or_else(|error| panic!("{error}"));
    println!("cargo:rerun-if-changed={}", binary_path.display());
    fs::write(
        out_dir.join("embedded_capnp.rs"),
        format!(
            "pub const CAPNP_BINARY: &[u8] = include_bytes!({:?});\n",
            binary_path.display().to_string()
        ),
    )
    .expect("the embedded compiler module is writable");
}

fn compile_fixtures(out_dir: &Path) {
    println!("cargo:rerun-if-changed={FIXTURES_DIR}");
    let capnp = build_helpers::host_capnp_for_execution();
    let mut command = capnpc::CompilerCommand::new();
    command
        .capnp_executable(capnp)
        .src_prefix(FIXTURES_DIR)
        .output_path(out_dir);
    for fixture in FIXTURES {
        command.file(format!("{FIXTURES_DIR}/{fixture}.capnp"));
    }
    command.run().expect("the fixture schemas compile");
}
