use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;
use std::sync::Arc;

pub fn setup() -> (
    tokio::runtime::Runtime,
    ServeCommandEmulation,
    Arc<AppContext>,
    tempfile::TempDir,
) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let work_dir = tempfile::tempdir().expect("failed to create temp work dir");
    let ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    (rt, serve, ctx, work_dir)
}
