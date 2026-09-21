mod capnp_build {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    /// Embed the bundled capnp binary for the target platform.
    ///
    /// The binary is the single source of truth shipped with `build-helpers`
    /// in the sealed tree (`peppy-shared/peppy-config-model/tools`).
    /// build-helpers locates the tools dir as its own sibling, so it is always
    /// present wherever the crate is: no cmake required.
    pub fn run() {
        let target = build_helpers::build_target_triple();
        let binary_path = build_helpers::bundled_capnp_for_embedding(&target)
            .unwrap_or_else(|error| panic!("{error}"));
        println!("cargo:rerun-if-changed={}", binary_path.display());

        let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
        let generated = out_dir.join("embedded_capnp.rs");
        let mut file = fs::File::create(&generated).unwrap();
        writeln!(
            file,
            r#"pub const CAPNP_BINARY: Option<&[u8]> = Some(include_bytes!("{}"));"#,
            binary_path.display()
        )
        .unwrap();
    }
}

fn main() {
    capnp_build::run();
}
