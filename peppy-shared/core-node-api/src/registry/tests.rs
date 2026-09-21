//! Enforcement tests for the method registry: wire-name pins, `(name, kind)`
//! uniqueness, `descriptor()` index math, schema-file coverage, and codec
//! spot-checks.

use std::collections::BTreeSet;

use super::*;

/// Read a descriptor's Cap'n Proto display name, e.g. `"clock.capnp:ClockRequest"`.
fn display_name(pd: &PayloadDescriptor) -> String {
    match (pd.introspect)().which() {
        capnp::introspect::TypeVariant::Struct(raw) => {
            let schema: capnp::schema::StructSchema = raw.into();
            schema
                .get_proto()
                .get_display_name()
                .expect("display name is present")
                .to_str()
                .expect("display name is UTF-8")
                .to_string()
        }
        _ => panic!("{} does not introspect to a struct", pd.rust_type),
    }
}

/// Pinned copy of every service wire string. Exhaustive on purpose:
/// adding a service without pinning its wire string here is a compile
/// error, so this second copy of the strings cannot be forgotten — its
/// only failure mode is a deliberate rename, which is exactly what
/// [`wire_names_are_pinned`] must force a human to look at (the strings
/// are the compatibility contract between the daemon and every client;
/// publish and subscribe sides must agree byte-for-byte).
fn pinned_service_name(id: ServiceId) -> &'static str {
    match id {
        ServiceId::Clock => "clock",
        ServiceId::ClockList => "clock_list",
        ServiceId::Info => "info",
        ServiceId::Health => "health",
        ServiceId::DatastoreStore => "datastore_store",
        ServiceId::DatastoreGet => "datastore_get",
        ServiceId::DatastoreList => "datastore_list",
        ServiceId::DatastoreRemove => "datastore_remove",
        ServiceId::StackReset => "stack_reset",
        ServiceId::StackList => "stack_list",
        ServiceId::ParticipantReserve => "participant_reserve",
        ServiceId::ParticipantSliceBegin => "participant_slice_begin",
        ServiceId::ParticipantInstancesRemove => "participant_instances_remove",
        ServiceId::PairCommit => "pair_commit",
        ServiceId::ParticipantRelease => "participant_release",
        ServiceId::RelationshipNotify => "relationship_notify",
        ServiceId::NodeInit => "node_init",
        ServiceId::NodeRemove => "node_remove",
        ServiceId::NodeSync => "node_sync",
        ServiceId::NodeInfo => "node_info",
        ServiceId::NodeStop => "node_stop",
        ServiceId::RepoAdd => "repo_add",
        ServiceId::RepoExclude => "repo_exclude",
        ServiceId::RepoList => "repo_list",
        ServiceId::RepoRemove => "repo_remove",
        ServiceId::ClockOffset => "clock_offset",
    }
}

/// See [`pinned_service_name`].
fn pinned_action_name(id: ActionId) -> &'static str {
    match id {
        ActionId::StackLaunch => "stack_launch",
        ActionId::StackJoin => "stack_join",
        ActionId::StackRemove => "stack_remove",
        ActionId::StackBenchmark => "stack_benchmark",
        ActionId::NodeAdd => "node_add",
        ActionId::NodeBuild => "node_build",
        ActionId::NodeRun => "node_run",
        ActionId::RepoRefresh => "repo_refresh",
    }
}

/// See [`pinned_service_name`].
fn pinned_topic_name(id: TopicId) -> &'static str {
    match id {
        TopicId::Clock => "clock",
        TopicId::DaemonHeartbeat => "daemon_heartbeat",
    }
}

/// An accidental variant rename (or a typo in a `name:` field) must not
/// silently change the wire; see [`pinned_service_name`].
#[test]
fn wire_names_are_pinned() {
    for &id in ServiceId::ALL {
        assert_eq!(id.name(), pinned_service_name(id));
    }
    for &id in ActionId::ALL {
        assert_eq!(id.name(), pinned_action_name(id));
    }
    for &id in TopicId::ALL {
        assert_eq!(id.name(), pinned_topic_name(id));
    }
}

/// `(name, kind)` is the registry key (`clock` is deliberately both a
/// service and a topic); two methods of one kind sharing a wire string
/// would shadow each other on the wire.
#[test]
fn method_name_kind_pairs_are_unique() {
    let pairs: BTreeSet<(&str, MethodKind)> = METHODS.iter().map(|m| (m.name, m.kind())).collect();
    assert_eq!(pairs.len(), METHODS.len(), "duplicate (name, kind) pair");
}

/// `descriptor()` indexes `METHODS` on the declaration-order invariant
/// (services, then actions, then topics); prove every id lands on its own
/// entry with the kind and host it claims.
#[test]
fn ids_index_their_own_descriptors() {
    assert_eq!(
        METHODS.len(),
        ServiceId::ALL.len() + ActionId::ALL.len() + TopicId::ALL.len(),
    );
    for &id in ServiceId::ALL {
        let m = id.descriptor();
        assert_eq!((m.name, m.kind()), (id.name(), MethodKind::Service));
        assert_eq!(m.host, id.host());
    }
    for &id in ActionId::ALL {
        let m = id.descriptor();
        assert_eq!((m.name, m.kind()), (id.name(), MethodKind::Action));
    }
    for &id in TopicId::ALL {
        let m = id.descriptor();
        assert_eq!((m.name, m.kind()), (id.name(), MethodKind::Topic));
    }
}

/// Every payload descriptor must introspect to a Cap'n Proto struct whose
/// display name is prefixed with the schema file the descriptor claims. The
/// `.src_prefix("schemas")` in `build.rs` guarantees the display name is
/// `"{file}.capnp:{TypePath}"`, so this ties each registry root to the right
/// `.capnp` file even where the Rust type name and capnp struct name differ
/// (e.g. `StackListRequest` -> `node.capnp:StackListRequest`).
#[test]
fn payload_descriptors_resolve_and_point_at_their_schema_file() {
    for m in METHODS {
        for pd in m.payloads.descriptors() {
            let display = display_name(pd);
            let prefix = format!("{}:", pd.schema_file);
            assert!(
                display.starts_with(&prefix),
                "{} ({}) introspects to {:?}, expected prefix {:?}",
                pd.rust_type,
                m.name,
                display,
                prefix,
            );
        }
    }
}

/// `SCHEMA_SOURCES` must key exactly the on-disk `schemas/*.capnp` files and
/// cover every `schema_file` referenced by a descriptor, with non-empty
/// contents.
#[test]
fn schema_sources_cover_the_schemas_dir() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/schemas");
    let on_disk: BTreeSet<String> = std::fs::read_dir(dir)
        .expect("schemas dir is readable")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .filter(|n| n.ends_with(".capnp"))
        .collect();

    let keys: BTreeSet<String> = SCHEMA_SOURCES.iter().map(|(k, _)| k.to_string()).collect();
    assert_eq!(
        keys, on_disk,
        "SCHEMA_SOURCES keys must equal schemas/*.capnp"
    );

    for (name, source) in SCHEMA_SOURCES {
        assert!(!source.is_empty(), "{name} source is empty");
    }

    for m in METHODS {
        for pd in m.payloads.descriptors() {
            assert!(
                keys.contains(pd.schema_file),
                "{} references schema {} which is not in SCHEMA_SOURCES",
                pd.rust_type,
                pd.schema_file,
            );
        }
    }
}

/// Spot-check that, for at least one payload per schema file, the hand-written
/// codec round-trips *and* the registry's introspected root points at exactly
/// the same capnp struct the codec name implies. These six are the empty,
/// trivially-constructible codecs (one per non-derived file plus the two
/// unions' files), so a codec silently repointing at another root shows up
/// here as a display-name mismatch.
#[test]
fn spot_check_codec_roundtrip_and_registry_root() {
    // Each is an empty unit-struct codec, so the bare type name is also a
    // value. `encode()` borrows, then `decode()` reconstructs from the bytes.
    macro_rules! roundtrip {
        ($ty:ident) => {{
            let value = crate::encoding::$ty;
            let payload = value.encode().expect("encode");
            let round = crate::encoding::$ty::decode(payload.as_ref()).expect("decode");
            assert_eq!(round, value, concat!(stringify!($ty), " round-trip"));
        }};
    }
    roundtrip!(HealthRequest);
    roundtrip!(InfoRequest);
    roundtrip!(RepoListRequest);
    roundtrip!(StackResetRequest);
    roundtrip!(ClockOffsetRequest);
    roundtrip!(RepoRefreshGoal);

    // Registry root points at exactly the named capnp struct (file:Type).
    let expected: &[(&MethodDescriptor, &str)] = &[
        (ServiceId::Health.descriptor(), "health.capnp:HealthRequest"),
        (ServiceId::Info.descriptor(), "info.capnp:InfoRequest"),
        (
            ServiceId::RepoList.descriptor(),
            "repo.capnp:RepoListRequest",
        ),
        (
            ServiceId::StackReset.descriptor(),
            "node.capnp:StackResetRequest",
        ),
        (
            ServiceId::ClockOffset.descriptor(),
            "clock.capnp:ClockOffsetRequest",
        ),
        (
            ActionId::RepoRefresh.descriptor(),
            "repo.capnp:RepoRefreshGoal",
        ),
    ];
    for (method, want) in expected {
        let pd = match &method.payloads {
            Payloads::Service { request, .. } => request,
            Payloads::Action { goal, .. } => goal,
            Payloads::Topic { message } => message,
        };
        assert_eq!(&display_name(pd), want, "{} root display name", method.name);
    }
}

/// `TypeId` handles are stable and distinct per codec struct (sanity check
/// that the `pd!` macro wired distinct types, not the same one twice).
#[test]
fn type_ids_are_distinct_per_payload() {
    let clock = ServiceId::Clock.descriptor();
    if let Payloads::Service { request, response } = &clock.payloads {
        assert_ne!((request.rust_type_id)(), (response.rust_type_id)());
    } else {
        panic!("clock service payloads");
    }
}
