#[cfg(test)]
mod frame_capnp;

use std::path::Path;

/// The output_dir should point to the `src` of a Rust crate. A new `capnp` module will be
/// created at the root of this directory with all the `capnp` files.
pub fn compile_capnp(capnp_files: &[impl AsRef<Path>], output_dir: impl AsRef<Path>) {
    let output_dir = output_dir.as_ref().to_path_buf();

    // Create capnp subdirectory
    let capnp_output_dir = output_dir.join("capnp");
    std::fs::create_dir_all(&capnp_output_dir).expect("Failed to create capnp output directory");

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
    command.output_path(&capnp_output_dir);

    // Set the default parent module to "capnp" so generated code references
    // crate::capnp::module_name instead of crate::module_name
    command.default_parent_module(vec!["capnp".to_string()]);

    // Determine common src_prefix if all files share a parent directory
    let common_parent = capnp_files
        .first()
        .and_then(|f| f.as_ref().parent())
        .filter(|p| !p.as_os_str().is_empty());

    if let Some(parent) = common_parent {
        command.src_prefix(parent);
    }

    // Add all files to the command
    for capnp_file in capnp_files {
        command.file(capnp_file.as_ref());
    }

    command.run().expect("capnp schema compilation failed");

    // Create capnp.rs module file that exports all generated modules
    let module_exports: Vec<String> = capnp_files
        .iter()
        .filter_map(|file| {
            let path = file.as_ref();
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|name| format!("pub mod {}_capnp;", name))
        })
        .collect();

    let capnp_rs_path = output_dir.join("capnp.rs");
    let capnp_rs_content = module_exports.join("\n") + "\n";
    std::fs::write(&capnp_rs_path, capnp_rs_content).expect("Failed to write capnp.rs module file");
}

// TODO 1: Convert the json5 types representation to a capn proto file
// TODO 2: Compile that file to a rust

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::frame_capnp::image_message;

    #[test]
    fn test_compile_capnp_schema() {
        let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join("frame.capnp");

        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let output_dir = temp_dir.path();

        assert!(
            schema_path.exists(),
            "Schema file should exist at {:?}",
            schema_path
        );

        compile_capnp(&[schema_path], output_dir);

        let expected_output = output_dir.join("capnp").join("frame_capnp.rs");
        assert!(
            expected_output.exists(),
            "Compiled output file should exist at {:?}",
            expected_output
        );

        let generated_content =
            std::fs::read_to_string(&expected_output).expect("Failed to read generated file");

        // Check that crate::capnp::frame_capnp:: is used for nested types
        assert!(
            generated_content.contains("crate::capnp::frame_capnp::"),
            "Generated code should use 'crate::capnp::frame_capnp::' for nested types"
        );

        // Check that capnp.rs module file was created
        let capnp_module_file = output_dir.join("capnp.rs");
        assert!(
            capnp_module_file.exists(),
            "capnp.rs module file should exist at {:?}",
            capnp_module_file
        );

        let capnp_module_content =
            std::fs::read_to_string(&capnp_module_file).expect("Failed to read capnp.rs file");

        // Check that it contains the correct module export
        assert!(
            capnp_module_content.contains("pub mod frame_capnp;"),
            "capnp.rs should contain 'pub mod frame_capnp;'"
        );
    }

    #[test]
    fn test_use_compiled_schema_types() {
        // Create a message builder
        let mut message = capnp::message::Builder::new_default();
        let mut img_msg = message.init_root::<image_message::Builder>();

        // Set some fields
        img_msg.set_width(1920);
        img_msg.set_height(1080);
        img_msg.set_encoding("rgb8");

        // Verify we can read it back
        let reader = img_msg.reborrow_as_reader();
        assert_eq!(reader.get_width(), 1920);
        assert_eq!(reader.get_height(), 1080);
    }

    #[test]
    fn test_encoding() {
        // TODO 1: Compile the `crates/config-internal/schemas/frame.capnp`
        // TODO 2: Below is a pseudo example of how
        let _kind_of_caller = r#"
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
