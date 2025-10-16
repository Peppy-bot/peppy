use std::path::Path;

pub fn compile_capnp(capnp_file: impl AsRef<Path>) {
    let capnp_file = capnp_file.as_ref().to_path_buf();

    let capnp_executable = {
        let binary_name = match std::env::consts::OS {
            "linux" if std::env::consts::ARCH == "x86_64" => "capnp_linux_x86_64",
            "macos" if std::env::consts::ARCH == "aarch64" => "capnp_macos_aarch64",
            _ => panic!(
                "unsupported platform: {}-{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        };

        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tools")
            .join(binary_name)
    };

    let mut command = capnpc::CompilerCommand::new();
    command.capnp_executable(capnp_executable);

    if let Some(parent) = capnp_file.parent().filter(|p| !p.as_os_str().is_empty()) {
        command.src_prefix(parent);
    }

    command
        .file(&capnp_file)
        .run()
        .expect("capnp schema compilation failed");
}

// TODO 1: Convert the json5 types representation to a capn proto file
// TODO 2: Compile that file to a rust

#[cfg(test)]
mod tests {
    fn test_encoding() {
        // TODO 1: Compile the `crates/config-internal/schemas/frame.capnp`
        // TODO 2: Below is a pseudo example of how
        let kind_of_caller = r#"
        #[derive(Debug, Clone)]
        pub struct PushFrameHeader {
            pub frame_id: u32,
            pub stamp: std::time::SystemTime,
        }

        pub async fn push_frame_async(
            encoding: String,
            header: PushFrameHeader,
            height: u32,
            image: [u8; 3],
            width: u32,
        ) {
            let _ = (&encoding, &header, &height, &image, &width);
            todo!("pass the parameters of the function to the ");
        }
        "#;
    }
}
