use config::consts::DAEMON_STATE_FILE_ENV;
use config::consts::{AppEnv, set_app_env};
use peppy::daemon_state::DaemonState;

struct EnvGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl Into<std::ffi::OsString>) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value.into()) };
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[test]
fn daemon_state_write_in_prod_uses_default_peppy_path() {
    set_app_env(AppEnv::Prod);

    let _state_file_guard = EnvGuard::remove(DAEMON_STATE_FILE_ENV);
    let temp_home = tempfile::tempdir().expect("temp home dir should create");
    let _home_guard = EnvGuard::set("HOME", temp_home.path().as_os_str());

    let daemon_state = DaemonState::new("master-node", config::consts::DEFAULT_MESSAGING_PORT);
    let written_path = daemon_state
        .write()
        .expect("daemon state should be writable without sudo");

    let expected_path = DaemonState::state_file_path();
    assert_eq!(
        written_path,
        expected_path,
        "expected daemon state to be written under ~/.peppy/daemon_state.json, got {}",
        written_path.display()
    );

    let read_back = DaemonState::read().expect("daemon state should be readable");
    assert_eq!(read_back.master_node_name, "master-node");
    assert_eq!(read_back.daemon_pid, Some(std::process::id()));
}
