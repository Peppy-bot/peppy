use std::fs;

use peppy::commands::service::install;

#[test]
fn install_peppy_command() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let override_dir = temp_dir.path().join("peppy_service");

    assert!(!override_dir.exists());

    let service_path = install::install_peppy_daemon(Some(override_dir.clone())).unwrap();

    assert!(override_dir.exists());
    assert!(service_path.exists());
    assert!(
        service_path.starts_with(&override_dir),
        "service path should be created inside the override directory"
    );

    let contents = fs::read_to_string(&service_path).expect("service definition readable");
    let current_exe = std::env::current_exe().unwrap();
    let working_dir_hint = current_exe
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".into());
    assert!(
        contents.contains(&working_dir_hint),
        "service definition should mention working directory {working_dir_hint}"
    );
    let exec_hint = current_exe.display().to_string();
    assert!(
        contents.contains(&exec_hint),
        "service definition should mention executable {exec_hint}"
    );

    #[cfg(target_os = "linux")]
    assert_eq!(service_path.file_name().unwrap(), "peppy-daemon.service");

    #[cfg(target_os = "macos")]
    assert_eq!(service_path.file_name().unwrap(), "bot.peppy.daemon.plist");
}
