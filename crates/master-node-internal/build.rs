use std::env;
use std::path::PathBuf;

fn get_capnp_binary() -> Option<PathBuf> {
    // First, check if capnp is available in PATH
    if let Ok(output) = std::process::Command::new("capnp")
        .arg("--version")
        .output()
    {
        if output.status.success() {
            return Some(PathBuf::from("capnp"));
        }
    }

    // Fall back to bundled binaries in config-internal crate
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok()?;
    let config_internal_tools = PathBuf::from(&manifest_dir)
        .parent()?
        .join("config-internal")
        .join("tools");

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let binary_name = "capnp_linux_x86_64";

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    let binary_name = "capnp_linux_aarch64";

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let binary_name = "capnp_macos_aarch64";

    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64")
    )))]
    let binary_name = "capnp_unsupported";

    let binary_path = config_internal_tools.join(binary_name);
    if binary_path.exists() {
        Some(binary_path)
    } else {
        None
    }
}

fn main() {
    println!("cargo:rerun-if-changed=schemas/");

    let capnp_path = get_capnp_binary().expect(
        "Could not find capnp binary. Please install Cap'n Proto: https://capnproto.org/install.html",
    );

    capnpc::CompilerCommand::new()
        .capnp_executable(capnp_path)
        .src_prefix("schemas")
        .file("schemas/messages.capnp")
        .run()
        .expect("Failed to compile Cap'n Proto schemas");
}
