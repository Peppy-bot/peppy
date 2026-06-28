mod capnp_build {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    /// Embed the bundled capnp binary for the host platform.
    ///
    /// The binary is the single source of truth shipped with `build-helpers`
    /// in public-peppy-libs (`peppyos-shared/peppy-config-model/tools`). peppyos
    /// pulls build-helpers as a cargo git dependency, so the tools dir is always
    /// present in the checkout — no superproject sibling and no cmake required.
    pub fn run() {
        let binary_path = build_helpers::bundled_capnp_path().unwrap_or_else(|| {
            panic!(
                "No bundled capnp binary for this host platform ({}/{}). Supported: \
                 linux x86_64/aarch64, macos aarch64. Add a binary to public-peppy-libs \
                 peppyos-shared/peppy-config-model/tools/.",
                env::consts::OS,
                env::consts::ARCH,
            )
        });
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
