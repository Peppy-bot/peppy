//! Boots `hello_receiver` in-process under the generated test harness and
//! drives one message through its consumed topic from the mocked producer:
//! no daemon, no real `hello_world_param` node, and no sleeps.

use peppygen::fixtures::harness::Harness;
use peppygen::mock::deps::hello_world_param::message_stream;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receives_a_message_from_the_mocked_producer() {
    let (harness, mocks) = Harness::start(hello_receiver::setup)
        .await
        .expect("the harness should boot the node");

    // The first publish waits for the node's subscription to match before
    // delivering, so this is deterministic: an `Ok` means the node received
    // the message, and no subscriber within the readiness timeout is a loud
    // error instead of a silent drop.
    mocks
        .deps
        .hello_world_param
        .message_stream
        .publish(&message_stream::Message {
            message: "hello from the mock".to_string(),
        })
        .await
        .expect("the node's subscription should receive the message");

    harness
        .shutdown()
        .await
        .expect("the node should shut down cleanly");
}
