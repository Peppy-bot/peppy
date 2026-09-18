//! Machine-readable registry of every core-node wire method.
//!
//! This is the single declaration point for the callable surface a federated
//! client sees over the zenoh wire: request/reply **services**, streaming
//! **actions**, and published **topics** (`clock` is both a service and a
//! topic, so entries are keyed by `(name, kind)`). Every method is declared
//! exactly once, in the `methods!` invocation below, which emits:
//!
//! * the [`ServiceId`] / [`ActionId`] / [`TopicId`] enums — the handles every
//!   consumer that must make a per-method decision (daemon registration,
//!   wire-name pinning, doc tagging) matches on **exhaustively**, so
//!   forgetting a new method is a compile error there;
//! * a [`ServiceRequest`] impl per routable service request codec and an
//!   [`ActionGoal`] impl per goal codec — the hooks `peppylib`'s generic
//!   `transport::poll` / `transport::send_goal` are bounded on, so a method
//!   declared here needs no per-method client wrapper anywhere;
//! * the [`METHODS`] descriptor slice, in declaration order, which the
//!   `platform-backend` AsyncAPI generator walks.
//!
//! Each entry hands out, per Cap'n Proto payload, three things:
//!
//! * a display name ([`PayloadDescriptor::rust_type`]),
//! * a [`TypeId`](core::any::TypeId) of the codec struct
//!   ([`PayloadDescriptor::rust_type_id`]) — a stable identity handle,
//!   sanity-checked by this module's tests, and
//! * a runtime Cap'n Proto reflection handle ([`PayloadDescriptor::introspect`])
//!   plus the originating `.capnp` file — consumed by the `platform-backend`
//!   AsyncAPI generator, which walks the type into JSON Schema.
//!
//! The registry describes the Cap'n Proto payloads *only*. The wire framing
//! around them (query/reply attachments, the goal-id envelope, the action
//! result-status byte) is documented by the generator, not here.
//!
//! Because the raw `.capnp` sources carry the doc comments that the compiled
//! schema nodes drop, they are embedded verbatim in [`SCHEMA_SOURCES`] and
//! served alongside the generated document.
//!
//! The descriptor types and the macros that expand this manifest live in
//! `machinery`; the enforcement tests (wire-name pins, ordering, schema
//! coverage) live in `tests`. This file holds only the declarations.

mod machinery;
#[cfg(test)]
mod tests;

pub use machinery::{
    ActionGoal, Host, LaunchScoped, MethodDescriptor, MethodKind, PayloadDescriptor, Payloads,
    ServiceRequest,
};
use machinery::{launch_scoped_impl, methods, pd, service_request_impl};

methods! {
    services {
        Clock {
            name: "clock",
            host: CoreNodeDaemon,
            summary: "Read the daemon's current wall-clock time snapshot.",
            request: ClockRequest,
            response: ClockResponse,
            schema: "clock.capnp",
        }
        Info {
            name: "info",
            host: CoreNodeDaemon,
            summary: "Report daemon build/runtime info and its container inventory.",
            request: InfoRequest,
            response: InfoResponse,
            schema: "info.capnp",
        }
        /// Liveness service the core node exposes for an external prober (the
        /// platform backend polls it over the federated zenoh link). Distinct
        /// from the per-node echo `node_health` in peppylib, which the
        /// daemon's watchdog uses to check spawned nodes.
        Health {
            name: "health",
            host: CoreNodeDaemon,
            summary: "Liveness probe: a well-formed reply is itself the health signal.",
            request: HealthRequest,
            response: HealthResponse,
            schema: "health.capnp",
        }
        DatastoreStore {
            name: "datastore_store",
            host: CoreNodeDaemon,
            summary: "Store a value under a datastore key.",
            request: DatastoreStoreRequest,
            response: DatastoreStoreResponse,
            schema: "datastore.capnp",
        }
        DatastoreGet {
            name: "datastore_get",
            host: CoreNodeDaemon,
            summary: "Fetch a value by datastore key.",
            request: DatastoreGetRequest,
            response: DatastoreGetResponse,
            schema: "datastore.capnp",
        }
        DatastoreList {
            name: "datastore_list",
            host: CoreNodeDaemon,
            summary: "List datastore entries (optionally under a prefix).",
            request: DatastoreListRequest,
            response: DatastoreListResponse,
            schema: "datastore.capnp",
        }
        DatastoreRemove {
            name: "datastore_remove",
            host: CoreNodeDaemon,
            summary: "Remove a datastore entry by key.",
            request: DatastoreRemoveRequest,
            response: DatastoreRemoveResponse,
            schema: "datastore.capnp",
        }
        StackReset {
            name: "stack_reset",
            host: CoreNodeDaemon,
            summary: "Tear down the whole node stack back to an empty state.",
            request: StackResetRequest,
            response: StackResetResponse,
            schema: "node.capnp",
        }
        StackList {
            name: "stack_list",
            host: CoreNodeDaemon,
            summary: "List every node in the stack with its lifecycle stage.",
            request: StackListRequest,
            response: StackListResponse,
            schema: "node.capnp",
        }
        /// Preflight step of a federated launch, called daemon-to-daemon.
        /// The coordinator holds a reservation on EVERY participant before
        /// anything is torn down, so two coordinators racing the same
        /// machines are refused before either has replaced a stack. The
        /// participant answers with what the coordinator cannot know: its
        /// version, its root identity, and the manifests for its own slice.
        ParticipantReserve {
            name: "participant_reserve",
            host: CoreNodeDaemon,
            summary: "Reserve this daemon for a federated launch and resolve its slice's manifests.",
            request: ParticipantReserveRequest,
            response: ParticipantReserveResponse,
            schema: "federation.capnp",
        }
        /// Commit point of a federated launch: the coordinator tells a
        /// reserved participant to tear down its stack, prepare the container
        /// bind sources its slice will need, and record that its new slice
        /// belongs to this launch. Split from the reservation because
        /// reserving must stay non-destructive until every participant has
        /// accepted.
        ParticipantSliceBegin {
            name: "participant_slice_begin",
            host: CoreNodeDaemon,
            summary: "Replace this daemon's stack slice on behalf of a reserved federated launch.",
            request: ParticipantSliceBeginRequest,
            response: ParticipantSliceBeginResponse,
            schema: "federation.capnp",
        }
        ParticipantInstancesRemove {
            name: "participant_instances_remove",
            host: CoreNodeDaemon,
            summary: "Remove selected instances from a reserved launch's slice.",
            request: ParticipantInstancesRemoveRequest,
            response: FederationVerdict,
            schema: "federation.capnp",
        }
        /// Records the second half of a CROSS-DAEMON pair on the daemon
        /// hosting the other endpoint, and delivers that endpoint its pin.
        /// A same-daemon pair never needs it: one registry holds both
        /// halves and one daemon delivers both pins.
        PairCommit {
            name: "pair_commit",
            host: CoreNodeDaemon,
            summary: "Record this daemon's half of a cross-daemon pair and pin its endpoint.",
            request: PairCommitRequest,
            response: FederationVerdict,
            schema: "federation.capnp",
        }
        /// Releases a reservation, whether because a later participant
        /// refused or because the launch finished. Idempotent.
        ParticipantRelease {
            name: "participant_release",
            host: CoreNodeDaemon,
            summary: "Release a federated-launch reservation held for a launch id.",
            request: ParticipantReleaseRequest,
            response: FederationVerdict,
            schema: "federation.capnp",
        }
        /// Runtime notification from the daemon that owns an instance to a
        /// daemon holding a relationship with it: a fresh incarnation for an
        /// observer to follow, or a death for a peer to dissolve against.
        /// Best-effort and idempotent by construction; the owning daemon
        /// stays authoritative, so a lost notification leaves the receiver
        /// stale rather than in disagreement.
        RelationshipNotify {
            name: "relationship_notify",
            host: CoreNodeDaemon,
            summary: "Notify a peer daemon that a related instance started a new incarnation or stopped.",
            request: RelationshipNotification,
            response: RelationshipNotificationAck,
            schema: "federation.capnp",
        }
        NodeInit {
            name: "node_init",
            host: CoreNodeDaemon,
            summary: "Initialize a node config entry in the stack.",
            request: NodeInitRequest,
            response: NodeInitResponse,
            schema: "node.capnp",
        }
        NodeRemove {
            name: "node_remove",
            host: CoreNodeDaemon,
            scope: launch,
            summary: "Remove a node entry from the stack.",
            request: NodeRemoveRequest,
            response: NodeRemoveResponse,
            schema: "node.capnp",
        }
        NodeSync {
            name: "node_sync",
            host: CoreNodeDaemon,
            summary: "Reconcile a node's config against its resolved repo sources.",
            request: NodeSyncRequest,
            response: NodeSyncResponse,
            schema: "node.capnp",
        }
        NodeInfo {
            name: "node_info",
            host: CoreNodeDaemon,
            summary: "Inspect a single node: config, stage, and tracked instances.",
            request: NodeInfoRequest,
            response: NodeInfoResponse,
            schema: "node.capnp",
        }
        /// The one service whose listener may be hosted by a per-instance
        /// node rather than the daemon: user node names are not unique
        /// across daemons, so daemon-root discovery alone cannot pin the
        /// route and `routing: bespoke` opts it out of the generated
        /// [`ServiceRequest`] impl. peppylib hosts its hand-written wrapper
        /// (`poll_node_stop`), which scopes discovery to the hosting core
        /// node.
        NodeStop {
            name: "node_stop",
            host: CoreNodeDaemon,
            routing: bespoke,
            summary: "Stop a running node instance.",
            request: NodeStopRequest,
            response: NodeStopResponse,
            schema: "node.capnp",
        }
        RepoAdd {
            name: "repo_add",
            host: CoreNodeDaemon,
            summary: "Register a repository source with the daemon.",
            request: RepoAddRequest,
            response: RepoAddResponse,
            schema: "repo.capnp",
        }
        RepoExclude {
            name: "repo_exclude",
            host: CoreNodeDaemon,
            summary: "Mark a repository as excluded from discovery.",
            request: RepoExcludeRequest,
            response: RepoExcludeResponse,
            schema: "repo.capnp",
        }
        RepoList {
            name: "repo_list",
            host: CoreNodeDaemon,
            summary: "List registered repositories and their discovered node entries.",
            request: RepoListRequest,
            response: RepoListResponse,
            schema: "repo.capnp",
        }
        RepoRemove {
            name: "repo_remove",
            host: CoreNodeDaemon,
            summary: "Remove a repository source.",
            request: RepoRemoveRequest,
            response: RepoRemoveResponse,
            schema: "repo.capnp",
        }
        /// Lists the clock domains this daemon hosts and the instances on it
        /// that read one, so `peppy clock list` can show every managed domain
        /// across a federation and a `--clock` reference can be resolved.
        ClockList {
            name: "clock_list",
            host: CoreNodeDaemon,
            summary: "List the clock domains this daemon hosts and its instances that read one.",
            request: ClockListRequest,
            response: ClockListResponse,
            schema: "clock.capnp",
        }
        /// Framework service every spawned node exposes.
        /// The daemon polls it during `peppy stack benchmark` to measure each
        /// producer's clock offset and normalize cross-host timestamps.
        ClockOffset {
            name: "clock_offset",
            host: SpawnedNode,
            summary: "Framework service every spawned node exposes; the daemon polls it \
                      during `peppy stack benchmark` to normalize cross-host timestamps.",
            request: ClockOffsetRequest,
            response: ClockOffsetResponse,
            schema: "clock.capnp",
        }
    }
    actions {
        StackLaunch {
            name: "stack_launch",
            summary: "Launch a stack from a launcher manifest, streaming per-node progress.",
            goal: LaunchGoal,
            goal_response: LaunchGoalResponse,
            feedback: LaunchFeedback,
            result: LaunchResult,
            schema: "launch.capnp",
        }
        StackJoin {
            name: "stack_join",
            summary: "Add a copy of one of the running launcher's options to the stack.",
            goal: StackJoinGoal,
            goal_response: LaunchGoalResponse,
            feedback: LaunchFeedback,
            result: LaunchResult,
            schema: "launch.capnp",
        }
        StackRemove {
            name: "stack_remove",
            summary: "Stop and remove the instances belonging to one copy.",
            goal: StackRemoveGoal,
            goal_response: LaunchGoalResponse,
            feedback: LaunchFeedback,
            result: LaunchResult,
            schema: "launch.capnp",
        }
        StackBenchmark {
            name: "stack_benchmark",
            summary: "Benchmark interface latencies across the running stack.",
            goal: StackBenchmarkGoal,
            goal_response: StackBenchmarkGoalResponse,
            feedback: StackBenchmarkFeedback,
            result: StackBenchmarkResult,
            schema: "benchmark.capnp",
        }
        NodeAdd {
            name: "node_add",
            scope: launch,
            summary: "Add a node to the stack, streaming resolution and fetch progress.",
            goal: NodeAddGoal,
            goal_response: NodeAddGoalResponse,
            feedback: NodeAddFeedback,
            result: NodeAddResult,
            schema: "node.capnp",
        }
        NodeBuild {
            name: "node_build",
            scope: launch,
            summary: "Build a node's container image, streaming build log lines.",
            goal: NodeBuildGoal,
            goal_response: NodeBuildGoalResponse,
            feedback: NodeBuildFeedback,
            result: NodeBuildResult,
            schema: "node.capnp",
        }
        NodeRun {
            name: "node_run",
            scope: launch,
            summary: "Run a node instance, streaming startup progress.",
            goal: NodeRunGoal,
            goal_response: NodeRunGoalResponse,
            feedback: NodeRunFeedback,
            result: NodeRunResult,
            schema: "node.capnp",
        }
        RepoRefresh {
            name: "repo_refresh",
            summary: "Rescan repositories, streaming each discovered/excluded item.",
            goal: RepoRefreshGoal,
            goal_response: RepoRefreshGoalResponse,
            feedback: RepoRefreshFeedback,
            result: RepoRefreshResult,
            schema: "repo.capnp",
        }
    }
    topics {
        Clock {
            name: "clock",
            summary: "Periodic wall-clock snapshot published for time-sync consumers.",
            message: ClockTick,
            schema: "clock.capnp",
        }
        /// Topic the daemon publishes a periodic liveness beat on. Each
        /// spawned node subscribes via its daemon-liveness watchdog and shuts
        /// itself down if the beat goes silent past the configured grace
        /// period (uncatchable-death safety net). Keyed by the core-node name
        /// (derived deterministically per machine unless the operator
        /// overrides it via `core_node_name` in
        /// `~/.peppy/conf/peppy_config.json5` or `--core-node-name`), so a
        /// restarted daemon resumes on the same key and nodes survive the
        /// restart.
        DaemonHeartbeat {
            name: "daemon_heartbeat",
            summary: "Daemon liveness beacon; the payload is a constant ClockTick and only arrival matters.",
            message: ClockTick,
            schema: "clock.capnp",
        }
    }
}

/// The raw `.capnp` sources, embedded so the compiled-away doc comments remain
/// downloadable. Keys are the bare file names (e.g. `"clock.capnp"`); the
/// [`PayloadDescriptor::schema_file`] fields index into this table. Kept in
/// sync with the on-disk `schemas/` directory by this module's tests.
pub const SCHEMA_SOURCES: &[(&str, &str)] = &[
    (
        "benchmark.capnp",
        include_str!("../schemas/benchmark.capnp"),
    ),
    ("clock.capnp", include_str!("../schemas/clock.capnp")),
    (
        "datastore.capnp",
        include_str!("../schemas/datastore.capnp"),
    ),
    (
        "federation.capnp",
        include_str!("../schemas/federation.capnp"),
    ),
    ("health.capnp", include_str!("../schemas/health.capnp")),
    ("info.capnp", include_str!("../schemas/info.capnp")),
    ("launch.capnp", include_str!("../schemas/launch.capnp")),
    ("node.capnp", include_str!("../schemas/node.capnp")),
    ("repo.capnp", include_str!("../schemas/repo.capnp")),
];

/// Version of the wire protocol described by this registry. Feeds the AsyncAPI
/// document's `info.version`. Bump the minor on additive methods/fields, the
/// major on breaking wire changes.
pub const WIRE_API_VERSION: &str = "0.3.0";

/// Version of this crate, for provenance only (not the wire version).
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");
