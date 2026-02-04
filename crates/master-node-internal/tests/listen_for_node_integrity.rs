mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, start_master_node_with_mock_messenger, write_peppy_json5,
};
use common::{NodeStartTestTimeouts, send_node_add_and_wait, send_node_start_and_wait};
use config::consts::NODE_CONFIG_FILE;
use config::node::Name;
use config::peppy_config::Name as InstanceName;
use config::runtime::{NodeInstance, RuntimeConfig};
use config::test_helpers;
use gix_url::Url as GitUrl;
use master_node::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use master_node::names;
use peppylib::ServiceMessenger;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use {
    httptest::Expectation, httptest::Server, httptest::matchers::request,
    httptest::responders::status_code,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_integrity_success() {
    todo!("Finish")
}
