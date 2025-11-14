use peppy::install::install_peppyd;

// Can be run from the command line with:
// cargo run --manifest-path <path_to_root_Cargo.toml> -- init <node_name>
#[test]
fn test_init_peppy_command() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let non_existent_path = temp_dir.path().join("new_folder");

    assert!(!non_existent_path.exists());

    let peppy_config_path = install_peppyd(&non_existent_path).unwrap();

    assert!(non_existent_path.exists());
    assert!(peppy_config_path.exists());
    assert_eq!(peppy_config_path.file_name().unwrap(), "peppy.json5");
}
