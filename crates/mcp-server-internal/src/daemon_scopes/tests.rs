//! The scopes of the daemon targets of a server, as the launch gives them,
//! and the entries of these targets behind a real endpoint.

use super::DaemonScopes;
use crate::bridges::{Route, prepare};
use crate::moves::MoveRules;
use crate::serve::{ServeError, register_tasks, register_tools};
use crate::test_support::LoopbackEndpoint;
use config::failure_kind::PeppyFailureKind;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use peppy_codec::consumer::ConsumerIdentity;
use peppy_mcp_catalog::build_exposure_bundle;
use peppy_mcp_runtime::ExposureServer;
use peppylib::messaging::{Failure, MessengerHandle, ProducerRef, SenderTarget};
use peppylib::testing::{
    EphemeralRouter, MockServiceCore, READINESS_TIMEOUT, wait_service_reachable,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const SERVER_INSTANCE: &str = "framework_controls_inst";

/// The scope `simulation_mcp` gives its framework endpoint: the OpenArm
/// offers its brain and a recorder, the SO-101 no feature.
fn simulation_scope() -> Value {
    json!({
        "max_copies": 4,
        "options": [
            {
                "option": "openarm_sim",
                "model": "openarm_v2",
                "description": "A simulated OpenArm v2 standing on the floor",
                "features": [
                    { "feature": "ai_brain", "axis": "brain", "option": "ai_brain_vla",
                      "description": "An AI brain that finds items by words" },
                    { "feature": "recorder", "axis": "recorder", "option": "lerobot_recorder",
                      "description": "A recorder of episodes" },
                ],
            },
            { "option": "so101_sim", "model": "so101",
              "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

fn targets() -> BTreeMap<String, DaemonInterface> {
    BTreeMap::from([
        ("stack".to_owned(), DaemonInterface::StackCopies),
        ("fleet".to_owned(), DaemonInterface::Fleet),
    ])
}

fn scopes(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn scope_refusal(scopes: &BTreeMap<String, Value>) -> String {
    match DaemonScopes::parse(SERVER_INSTANCE, &targets(), scopes) {
        Ok(_) => panic!("the server must not start"),
        Err(error @ ServeError::DaemonScopes { .. }) => error.to_string(),
        Err(other) => panic!("expected a scope refusal, got {other}"),
    }
}

/// A server whose daemon targets have no scope stops, and says which
/// targets lack one and how to give it.
#[test]
fn a_server_without_a_scope_does_not_start() {
    assert_eq!(
        scope_refusal(&scopes(&[("fleet", json!({ "members": ["list"] }))])),
        "the daemon targets cannot be served:\n  - daemon target `stack` (stack_copies:v1) has \
         no scope; give it one in the `daemon_scopes` of instance `framework_controls_inst`, or \
         with a `set_daemon_scopes` adjustment on `framework_controls_inst` in the launcher\nA \
         launch and a join check every scope before they start the server; a server started \
         another way, for example with `peppy node run`, gets its scopes from the runtime \
         configuration of its instance only"
    );
}

/// A scope that does not parse and a scope that names no daemon target stop
/// the server, every problem at once.
#[test]
fn a_server_with_a_bad_scope_does_not_start() {
    let refusal = scope_refusal(&scopes(&[
        ("stack", json!({ "max_copies": 4, "options": [] })),
        ("fleet", json!({ "members": ["fly"] })),
        ("recorder", json!({})),
    ]));
    for expected in [
        "\n  - the scope of daemon target `stack` (stack_copies:v1) does not parse: `options` \
         names no option",
        "; correct it in the `daemon_scopes` of instance `framework_controls_inst`, or in the \
         `set_daemon_scopes` adjustment on `framework_controls_inst` in the launcher",
        "\n  - the scope of daemon target `fleet` (fleet:v1) does not parse",
        "\n  - `daemon_scopes.recorder` of instance `framework_controls_inst` names no daemon \
         target of this server; its daemon targets: `fleet`, `stack`",
    ] {
        assert!(
            refusal.contains(expected),
            "{expected}\nmissing from: {refusal}"
        );
    }

    DaemonScopes::parse(
        SERVER_INSTANCE,
        &targets(),
        &scopes(&[
            ("stack", simulation_scope()),
            ("fleet", json!({ "vacant": "the stack has one robot" })),
        ]),
    )
    .expect("scopes that parse start the server");
}

// --- The daemon targets behind a real endpoint.

const CORE_NODE: &str = "cn-desk";
/// The per-boot instance id of the daemon that serves the interfaces.
const DAEMON: &str = "core-boot";

/// The design's `framework/framework_controls.json5`.
const FRAMEWORK_CONTROLS: &str = include_str!(
    "../../../daemon-config-internal/src/daemon_interface/fixtures/framework_controls.json5"
);

/// `framework_controls:v1` served as `serve` serves it, on a loopback port:
/// validated against the interfaces, prepared, its schemas narrowed by
/// the scopes, and each entry bound to the daemon that serves the
/// interfaces on the mesh.
async fn framework_controls_endpoint(
    server: &MessengerHandle,
    scopes: &BTreeMap<String, Value>,
) -> LoopbackEndpoint {
    let document =
        PeppyMcpExposureParser::from_content(FRAMEWORK_CONTROLS).expect("the fixture parses");
    let validated = build_exposure_bundle(
        &document,
        &[],
        &[
            DaemonInterface::StackCopies.resolved(),
            DaemonInterface::Fleet.resolved(),
        ],
    )
    .expect("the fixture validates against the interfaces");
    let mut exposure = prepare(vec![validated])
        .expect("the exposure prepares")
        .remove(0);
    DaemonScopes::parse(SERVER_INSTANCE, &targets(), scopes)
        .expect("the scopes parse")
        .narrow(&mut exposure.bundle);
    exposure.drop_unpublished_entries();
    let identity = ConsumerIdentity {
        core_node: CORE_NODE.to_owned(),
        instance_id: SERVER_INSTANCE.to_owned(),
    };
    let daemon = ProducerRef::new(CORE_NODE, DAEMON);
    let route = Route {
        messenger: server.clone(),
        identity: identity.clone(),
        bound_producer: {
            let daemon = daemon.clone();
            Arc::new(move |_target| daemon.clone())
        },
        rules: MoveRules::new(server.clone(), identity, daemon).expect("the rules lay out"),
    };
    let builder = ExposureServer::builder(exposure.bundle);
    let builder = register_tools(builder, exposure.tools, &route);
    let builder = register_tasks(builder, exposure.tasks, &route);
    LoopbackEndpoint::serve(builder.build().expect("the catalog and the handlers agree")).await
}

/// The scope narrows the input of each entry of a daemon target, so the
/// runtime refuses input outside it before a call reaches the daemon; a
/// vacant target serves no entry; and a call in the scope reaches the
/// daemon's interface as the call of a contract does, with its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_entries_of_a_daemon_target_reach_the_daemon_within_their_scope() {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let daemon_session = router.connect().await.expect("the daemon connects");
    let server = router.connect().await.expect("the server connects");
    let stack_copies = SenderTarget::contract("stack_copies", "v1").expect("a valid identity");
    let list = MockServiceCore::listen(
        &daemon_session,
        CORE_NODE,
        DAEMON,
        stack_copies.clone(),
        "list",
    )
    .await
    .expect("the daemon serves list");
    wait_service_reachable(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        stack_copies,
        "list",
        &ProducerRef::new(CORE_NODE, DAEMON),
        READINESS_TIMEOUT,
    )
    .await
    .expect("list becomes reachable");
    let endpoint = framework_controls_endpoint(
        &server,
        &scopes(&[
            ("stack", simulation_scope()),
            ("fleet", json!({ "vacant": "the stack has one robot" })),
        ]),
    )
    .await;

    let outside = endpoint
        .answer_of(
            "stack.join",
            json!({ "name": "bravo", "option": "web_commander", "core_node": CORE_NODE, "features": [] }),
        )
        .await;
    assert!(
        outside.get("error").is_some(),
        "an option outside the scope is refused: {outside}"
    );
    let vacant = endpoint.answer_of("fleet.halt", json!({})).await;
    assert!(
        vacant.get("error").is_some() || vacant["result"]["isError"] == true,
        "a vacant target serves no entry: {vacant}"
    );
    assert!(
        list.captured().is_empty(),
        "no refused call reaches the daemon"
    );

    let (listed, ()) = tokio::join!(endpoint.answer_of("stack.list", json!({})), async {
        let (_request, responder) = list
            .next_request(READINESS_TIMEOUT)
            .await
            .expect("the call reaches the daemon");
        responder
            .respond_failure(Failure::new(
                PeppyFailureKind::PreconditionFailed,
                "this machine runs a slice of launch `launch-1`, which `cn-robot` coordinates",
            ))
            .await
            .expect("the daemon answers");
    });
    assert_eq!(listed["result"]["isError"], true, "{listed}");
    let text = listed["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("which `cn-robot` coordinates"),
        "the answer of the daemon reaches the client: {listed}"
    );
    endpoint.stop().await;
}
