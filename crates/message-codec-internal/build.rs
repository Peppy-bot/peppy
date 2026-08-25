//! Compiles the fixture schemas under `tests/fixtures` into typed Cap'n
//! Proto accessors, the "generated codec" the typed round-trip test holds
//! the runtime codec against. The schemas are the renderer's output for the
//! fixture formats, as `tests/schema_golden.rs` pins.

use std::env;
use std::path::PathBuf;

const FIXTURES_DIR: &str = "tests/fixtures";
const FIXTURES: &[&str] = &["everything", "frame"];

fn main() {
    println!("cargo:rerun-if-changed={FIXTURES_DIR}");

    let capnp = build_helpers::host_capnp_for_execution();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    let mut command = capnpc::CompilerCommand::new();
    command
        .capnp_executable(capnp)
        .src_prefix(FIXTURES_DIR)
        .output_path(&out_dir);
    for fixture in FIXTURES {
        command.file(format!("{FIXTURES_DIR}/{fixture}.capnp"));
    }
    command.run().expect("the fixture schemas compile");
}
