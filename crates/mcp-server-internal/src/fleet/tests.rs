//! The ingests the follow loop keeps in step with the stack: a member
//! joining routes every resource of its target to it, and one leaving drops
//! them, while the pumps' own subscriptions follow the bound producers.

use super::*;
use peppy_mcp_catalog::ExposureBundle;
use peppy_mcp_runtime::ExposureServer;

const CORE_NODE: &str = "test_core";

/// A per-robot bundle whose status target publishes one topic as two
/// resources and whose camera target a robot fills any number of times.
fn bundle() -> ExposureBundle {
    ExposureBundle::from_json_str(
        r#"{
  "bundle_format": 1,
  "schema_mapping_version": 1,
  "exposure": { "name": "robot_control", "tag": "v1" },
  "server": { "title": "Robots" },
  "robots": {
    "list": { "name": "robot.list", "description": "The robots of the stack." }
  },
  "contracts": [
    { "name": "robot_status", "tag": "v1", "sha256": "aa", "link_id": "status" },
    { "name": "rgb_camera", "tag": "v1", "sha256": "bb", "link_id": "camera", "argument": "camera" }
  ],
  "resources": [
    {
      "name": "robot.status",
      "uri": "peppy://resource/robot.status",
      "description": "The robot's latest status.",
      "target": "status",
      "member": "status",
      "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 2.0 } },
      "schema": { "type": "object" }
    },
    {
      "name": "robot.status_text",
      "uri": "peppy://resource/robot.status_text",
      "description": "The same status, rendered for a reader.",
      "target": "status",
      "member": "status",
      "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 1.0 } },
      "schema": { "type": "object" }
    },
    {
      "name": "camera.latest_frame",
      "uri": "peppy://resource/camera.latest_frame",
      "description": "The camera's latest frame.",
      "target": "camera",
      "member": "video_stream",
      "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 2.0 } },
      "schema": { "type": "object" }
    }
  ],
  "tools": [],
  "tasks": []
}"#,
    )
    .expect("the per-robot bundle parses")
}

fn member(target: &str, robot: &str, name: &str) -> FleetMember {
    FleetMember {
        target: target.to_owned(),
        address: MemberAddress {
            core_node: CORE_NODE.to_owned(),
            instance_id: format!("{robot}_{name}"),
        },
        robot: Some(robot.to_owned()),
        name: name.to_owned(),
    }
}

/// A handle over a fleet holding one status member and one camera member of
/// robot `alpha`.
fn handle() -> FleetHandle {
    let fleet = vec![
        member("status", "alpha", "backbone_inst"),
        member("camera", "alpha", "wrist_left"),
    ];
    ExposureServer::builder(bundle())
        .with_fleet(move || fleet.clone())
        .build()
        .expect("the bundle exposes no tool to register")
        .fleet()
        .expect("a per-robot server")
}

/// One empty ingest table per resource entry of each target, as
/// [`start_pumps`] hands them back.
fn ingests() -> HashMap<String, TargetIngests> {
    HashMap::from([
        (
            "status".to_owned(),
            TargetIngests::from([
                ("robot.status".to_owned(), EntryIngests::default()),
                ("robot.status_text".to_owned(), EntryIngests::default()),
            ]),
        ),
        (
            "camera".to_owned(),
            TargetIngests::from([("camera.latest_frame".to_owned(), EntryIngests::default())]),
        ),
    ])
}

/// The instances whose messages fill one entry's resources.
fn routed(ingests: &HashMap<String, TargetIngests>, target: &str, entry: &str) -> Vec<String> {
    let mut instances: Vec<String> = ingests[target][entry]
        .read()
        .expect(INGEST_LOCK)
        .keys()
        .map(|producer| producer.instance_id.clone())
        .collect();
    instances.sort();
    instances
}

#[test]
fn a_member_routes_every_resource_entry_of_its_target_alone() {
    let handle = handle();
    let ingests = ingests();

    assert!(attach(
        &handle,
        &ingests,
        &member("status", "alpha", "backbone_inst")
    ));
    // One topic published as two resources feeds both from the one member.
    assert_eq!(
        routed(&ingests, "status", "robot.status"),
        ["alpha_backbone_inst"]
    );
    assert_eq!(
        routed(&ingests, "status", "robot.status_text"),
        ["alpha_backbone_inst"]
    );
    assert!(routed(&ingests, "camera", "camera.latest_frame").is_empty());

    assert!(attach(
        &handle,
        &ingests,
        &member("camera", "alpha", "wrist_left")
    ));
    assert_eq!(
        routed(&ingests, "camera", "camera.latest_frame"),
        ["alpha_wrist_left"]
    );
}

#[test]
fn a_member_leaving_drops_the_ingests_of_its_target_alone() {
    let handle = handle();
    let ingests = ingests();
    let status = member("status", "alpha", "backbone_inst");
    let camera = member("camera", "alpha", "wrist_left");
    attach(&handle, &ingests, &status);
    attach(&handle, &ingests, &camera);

    assert!(detach(&handle, &ingests, &status));
    assert!(routed(&ingests, "status", "robot.status").is_empty());
    assert!(routed(&ingests, "status", "robot.status_text").is_empty());
    assert_eq!(
        routed(&ingests, "camera", "camera.latest_frame"),
        ["alpha_wrist_left"]
    );

    // The member is gone, so a second pass finds nothing to drop and the
    // resource list is left alone.
    assert!(!detach(&handle, &ingests, &status));
}

#[test]
fn a_member_the_fleet_does_not_route_to_publishes_nothing() {
    let handle = handle();
    let ingests = ingests();

    // `bravo` is not in the fleet the source reports, so no call reaches it
    // and it publishes no resource.
    assert!(!attach(
        &handle,
        &ingests,
        &member("status", "bravo", "backbone_inst")
    ));
    assert!(routed(&ingests, "status", "robot.status").is_empty());
}
