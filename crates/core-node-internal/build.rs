use std::env;
use std::path::PathBuf;

fn main() {
    build_helpers::embed_git_tag();

    println!("cargo:rerun-if-changed=schemas/");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let tools_dir = manifest_dir.parent().unwrap().join("config-internal").join("tools");
    let capnp_path = build_helpers::find_bundled_capnp(&tools_dir).expect(
        "Could not find capnp binary. Please install Cap'n Proto: https://capnproto.org/install.html",
    );

    let schemas_dir = manifest_dir.join("schemas");
    for entry in std::fs::read_dir(&schemas_dir).expect("Failed to read schemas directory") {
        let entry = entry.expect("Failed to read schema directory entry");
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "capnp") {
            capnpc::CompilerCommand::new()
                .capnp_executable(capnp_path.clone())
                .src_prefix("schemas")
                .file(&path)
                .run()
                .unwrap_or_else(|e| panic!("Failed to compile {}: {}", path.display(), e));
        }
    }
}
