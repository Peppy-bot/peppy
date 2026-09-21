use std::path::Path;

use crate::error::{Error, ParameterDeserializationError, Result};
use crate::messaging::{ObservationState, ObservedMemberState, PeerMember, PeerSetState};
use config::{
    AnyType, NodeArguments,
    consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME},
    node::{
        Cardinality, EndpointLabel, NodeConfig, PairingObserverDependency,
        load_standalone_node_config,
    },
    runtime::{Name, NodeInstanceConfig, RuntimeConfig},
    validate_node_arguments,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::watch;

use super::builder::StandaloneConfig;

/// The core-node identity segment every standalone (daemon-less) node runs
/// under. Generated test surfaces pin subscriptions and producer refs to the
/// node-under-test with this same value (re-exported from
/// [`crate::testing`]), so the mock's view of the node and the runtime's own
/// cannot fork.
pub const STANDALONE_CORE_NODE: &str = "standalone-core";

/// Runtime processor that holds configuration for the node.
#[derive(Clone)]
pub struct Processor {
    runtime_config: RuntimeConfig,
    validated_arguments: NodeArguments,
    /// The validated bound producer set per declared `link_id`, sized per
    /// the slot's declared cardinality (the [`BoundProducers`] value type
    /// carries the ordered / duplicate-free invariants). Computed once at
    /// startup from the daemon-supplied `slot_bindings` plus the manifest's
    /// `depends_on`; immutable for the node's lifetime (a producer
    /// disconnecting never shrinks it), and cached so subscribe / poll /
    /// send_goal call sites return a borrowed slice cheaply.
    ///
    /// [`BoundProducers`]: config::runtime::BoundProducers
    bound_producers: BTreeMap<String, config::runtime::BoundProducers>,
    /// One live pairing-slot channel per `depends_on.pairings` entry, keyed
    /// by the slot's link_id. The map's key set is fixed at startup (slots
    /// are declared in the manifest); only the channel values move: the
    /// daemon replaces them over the `peer_update` service, and per-slot
    /// `PeerSubscription`s, `PeerSlot`s, `PeerSlotSet`s and `PeerPublisher`s
    /// observe them. Behind an `Arc` so `Processor::clone` shares the live
    /// channels instead of forking them.
    pairing_slots: Arc<BTreeMap<String, watch::Sender<PeerSetState>>>,
    /// Each pairing slot's declared `cardinality`, keyed by link_id and read
    /// from the same manifest entries that seed `pairing_slots`. It picks the
    /// slot's handle (`peer` vs `peer_set`) and which accessor that slot's
    /// generated module calls.
    pairing_cardinalities: BTreeMap<String, Cardinality>,
    /// One live observation-slot channel per `depends_on.pairing_observers`
    /// entry, keyed by the slot's link_id. Same lifecycle shape as
    /// `pairing_slots`: the key set is fixed at startup and the daemon mutates
    /// the channel values over the `observation_update` service, while per-slot
    /// `ObservedTopicSubscription`s / `ObservationSlot`s observe them.
    observation_slots: Arc<BTreeMap<String, watch::Sender<ObservationState>>>,
    /// Each observer slot's declared `cardinality`, keyed by link_id and read
    /// from the same manifest entries that seed `observation_slots`. It picks
    /// the slot's handle (`observation_slot` vs `observation_slot_set`) and,
    /// through the handle, which accessor that slot's generated module calls; it
    /// also gates the slot's seed at startup, so a floored slot cannot boot
    /// observing less than its manifest declares.
    observation_cardinalities: BTreeMap<String, Cardinality>,
    /// The instant this instance's clock last saw, shared by every handle that
    /// reads or writes it. A consumer's subscription feeds it; a publisher's
    /// `publish` commits to it and reads it straight back, which is how a
    /// source reports its own time without waiting for a transport echo.
    /// `0` is "no tick yet", so a read before the first one is not ready.
    /// Unused under wall time, where the OS clock answers.
    clock_cache: Arc<AtomicU64>,
    /// The labels the manifest declares under `execution.endpoints`: the set
    /// [`crate::runtime::NodeRunner::announce_endpoint`] admits, and the set
    /// setup must cover before the runtime seals it.
    endpoint_labels: BTreeSet<EndpointLabel>,
}

impl Processor {
    /// Create processor for daemon mode.
    ///
    /// Resolves the launch-config path from the `PEPPY_RUNTIME_CONFIG` env var
    /// (set by the CLI when it launches the node) and delegates to
    /// [`new_daemon_from_path`](Self::new_daemon_from_path). Validates the
    /// fingerprint matches compiled code.
    pub fn new_daemon(peppy_config: impl AsRef<Path>) -> Result<Self> {
        Self::new_daemon_from_path(peppy_config, &Self::launch_config_path_from_env()?)
    }

    /// Create processor for daemon mode from a manifest held in memory.
    ///
    /// For a node that ships no generated bindings and builds its manifest
    /// itself: there is no codegen output whose fingerprint could be checked
    /// against the manifest, so the manifest is taken as given. The launch
    /// config comes from the `PEPPY_RUNTIME_CONFIG` env var, as in
    /// [`new_daemon`](Self::new_daemon).
    pub fn new_daemon_with_manifest(node_config: NodeConfig) -> Result<Self> {
        let runtime_config = Self::load_runtime_config(&Self::launch_config_path_from_env()?)?;
        Self::daemon_from_manifest(runtime_config, node_config)
    }

    fn launch_config_path_from_env() -> Result<String> {
        std::env::var(RUNTIME_CONFIG_VAR_NAME).map_err(|source| Error::MissingInstanceIdEnvVar {
            var: RUNTIME_CONFIG_VAR_NAME,
            source,
        })
    }

    /// Create processor for daemon mode from an explicit launch-config path.
    ///
    /// This is the env-free core that [`new_daemon`](Self::new_daemon) wraps:
    /// the launch-config path is passed in rather than read from the
    /// environment, so callers that already know the path (embedders, tests)
    /// can construct a daemon-mode processor without touching process state.
    /// Validates the fingerprint matches compiled code.
    pub fn new_daemon_from_path(
        peppy_config: impl AsRef<Path>,
        launch_config_path: &str,
    ) -> Result<Self> {
        let runtime_config = Self::load_runtime_config(launch_config_path)?;

        let codegen_fingerprint = config::fingerprint::read_codegen_fingerprint(
            peppy_config.as_ref(),
            PEPPYGEN_OUTPUT_PATH,
        )
        .map_err(|source| Error::CodegenFingerprintRead {
            path: peppy_config.as_ref().display().to_string(),
            source: std::io::Error::other(source.to_string()),
        })?;
        Self::validate_fingerprint(peppy_config.as_ref(), &codegen_fingerprint)?;

        let node_config: NodeConfig =
            serde_json5::from_str(&std::fs::read_to_string(peppy_config.as_ref())?)?;
        Self::daemon_from_manifest(runtime_config, node_config)
    }

    /// The daemon-mode core every daemon constructor ends in: validates the
    /// launch arguments against the manifest's parameter schema and builds
    /// the slot caches from the launch config's bindings and the manifest's
    /// `depends_on`. Public so an embedder holding both documents in memory
    /// builds the same processor a spawned node builds from its files.
    pub fn daemon_from_manifest(
        runtime_config: RuntimeConfig,
        node_config: NodeConfig,
    ) -> Result<Self> {
        let validated_arguments = validate_node_arguments(
            runtime_config.node_instance.arguments.clone(),
            &node_config.execution.parameters,
        )?;

        let bound_producers = build_bound_producers(&runtime_config, &node_config)?;
        let pairing_cardinalities = build_pairing_cardinalities(&node_config);
        let pairing_slots = build_pairing_slots(&runtime_config, &pairing_cardinalities)?;
        let observation_cardinalities = build_observer_cardinalities(&node_config);
        let observation_slots =
            build_observation_slots(&runtime_config, &observation_cardinalities)?;

        Ok(Self {
            runtime_config,
            validated_arguments,
            bound_producers,
            pairing_slots,
            pairing_cardinalities,
            observation_slots,
            observation_cardinalities,
            clock_cache: Arc::new(AtomicU64::new(0)),
            endpoint_labels: declared_endpoint_labels(&node_config),
        })
    }

    /// Create processor for standalone mode.
    ///
    /// Uses provided configuration or defaults:
    /// - messaging_host: DEFAULT_ZENOH_HOST or user-provided
    /// - messaging_port: DEFAULT_ZENOH_PORT or user-provided
    /// - instance_id: "standalone" or user-provided
    /// - node_name: from peppy.json5 or user-provided
    ///
    /// Skips fingerprint validation for development flexibility.
    pub fn new_standalone(
        peppy_config: impl AsRef<Path>,
        config: &StandaloneConfig,
    ) -> Result<Self> {
        let node_config: NodeConfig = load_standalone_node_config(peppy_config.as_ref())?;
        Self::standalone_from_manifest(node_config, config)
    }

    /// [`new_standalone`](Self::new_standalone) for a manifest held in
    /// memory rather than read from `peppy.json5`.
    pub fn standalone_from_manifest(
        node_config: NodeConfig,
        config: &StandaloneConfig,
    ) -> Result<Self> {
        let arguments: BTreeMap<String, AnyType> = match &config.parameters {
            Some(params) => serde_json::from_value(params.clone()).map_err(|e| {
                ParameterDeserializationError::single(format!("failed to parse parameters: {}", e))
            })?,
            None => BTreeMap::new(),
        };

        let validated_arguments =
            validate_node_arguments(arguments, &node_config.execution.parameters)?;

        let node_name: String = config
            .node_name
            .clone()
            .unwrap_or_else(|| node_config.manifest.name.clone().into());

        let instance_id = config
            .instance_id
            .clone()
            .unwrap_or_else(|| "standalone".to_string());

        let messaging_host = config.messaging_host_or_default();
        let messaging_port = config.messaging_port_or_default();

        let instance_id_name =
            Name::new(instance_id.clone()).map_err(|e| Error::InvalidNodeName {
                node_name: instance_id,
                reason: e.to_string(),
            })?;

        // Daemon-less development: `StandaloneConfig::with_bound_producer`
        // seeds a consumer slot's producer set, standing in for the
        // launcher's validated binding map. Undeclared link_ids are ignored
        // with a warning (mirroring `peer_pins` below); a declared slot
        // whose seeded set violates its cardinality fails
        // `build_bound_producers`, exactly like a daemon boot config with a
        // bad binding. Duplicate seeded producers are rejected here, where
        // `BoundProducers::try_from` mirrors the boot-config parse rule.
        let mut slot_bindings = config::runtime::SlotBindings::new();
        let declared_slots: std::collections::BTreeSet<&str> = node_config
            .manifest
            .depends_on
            .as_ref()
            .map(|deps| {
                deps.nodes
                    .iter()
                    .map(|dep| dep.link_id.as_str())
                    .chain(deps.contracts.iter().map(|dep| dep.link_id.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        for (link_id, producers) in &config.bound_producers {
            if !declared_slots.contains(link_id.as_str()) {
                tracing::warn!(
                    link_id = %link_id,
                    "StandaloneConfig bound producer names an undeclared consumer slot; ignoring"
                );
                continue;
            }
            let bound = config::runtime::BoundProducers::try_from(producers.clone())
                .map_err(config::ConfigError::from)?;
            slot_bindings.insert(link_id.clone(), bound);
        }

        // Daemon-less development: `StandaloneConfig::with_observed_source`
        // seeds an observer slot's member set, standing in for the set the
        // daemon stamps into a spawned node's boot config. Undeclared link_ids
        // are ignored with a warning, on the same terms as `bound_producers`
        // above; a declared slot whose seeded set violates its cardinality, or
        // that repeats a pairing, fails `build_observation_slots`, exactly like
        // a daemon boot config with a bad seed. Members boot live at generation
        // zero: standalone has no daemon to report an incarnation change or a
        // source going down.
        let observation_cardinalities = build_observer_cardinalities(&node_config);
        let mut observation_seeds = config::runtime::ObservationSeeds::new();
        for (link_id, sources) in &config.observed_sources {
            if !observation_cardinalities.contains_key(link_id) {
                tracing::warn!(
                    link_id = %link_id,
                    "StandaloneConfig observed source names an undeclared observer slot; ignoring"
                );
                continue;
            }
            observation_seeds.insert(
                link_id.clone(),
                sources
                    .iter()
                    .map(|source| config::runtime::ObservationSeedMember {
                        source: source.producer.clone(),
                        source_link_id: source.source_link_id.clone(),
                        peer: source
                            .peer
                            .as_ref()
                            .map(config::runtime::ObservedPeer::from),
                        source_generation: 0,
                        source_live: true,
                    })
                    .collect(),
            );
        }

        // Daemon-less development: `StandaloneConfig::with_peer_pin` seeds a
        // pairing slot's pairs, standing in for the set the daemon stamps
        // into a spawned node's boot config. Undeclared link_ids are ignored
        // with a warning, on the same terms as `bound_producers` above; a
        // scalar slot seeded with two pairs fails `build_pairing_slots`.
        let pairing_cardinalities = build_pairing_cardinalities(&node_config);
        let mut pairing_slots_seed = config::runtime::PairingSlots::new();
        for (link_id, pins) in &config.peer_pins {
            if !pairing_cardinalities.contains_key(link_id) {
                tracing::warn!(
                    link_id = %link_id,
                    "StandaloneConfig peer pin names an undeclared pairing slot; ignoring"
                );
                continue;
            }
            let pairs = pins
                .iter()
                .map(|pin| {
                    let copy = pin
                        .copy
                        .as_deref()
                        .map(|copy| {
                            Name::new(copy).map_err(|e| Error::InvalidCopyName {
                                link_id: link_id.clone(),
                                copy: copy.to_string(),
                                reason: e.to_string(),
                            })
                        })
                        .transpose()?;
                    Ok(config::runtime::PairedPeer {
                        peer: pin.info.producer.clone(),
                        peer_link_id: pin.info.peer_link_id.clone(),
                        copy,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            pairing_slots_seed.insert(link_id.clone(), pairs);
        }

        let runtime_config = RuntimeConfig::new(
            &messaging_host,
            messaging_port,
            NodeInstanceConfig {
                slot_bindings,
                observation_seeds,
                pairing_slots: pairing_slots_seed,
                // Daemon-less stand-in for a launcher's clock binding,
                // written to the same resolved `framework` field a daemon
                // launch fills, so neither `clock::for_node` nor
                // `ClockPublisher::for_node` needs a standalone branch.
                framework: config::runtime::ResolvedFramework {
                    clock: config.clock.clone(),
                },
                ..NodeInstanceConfig::new(instance_id_name)
            },
            &node_name,
            node_config.manifest.tag.as_str(),
            STANDALONE_CORE_NODE,
        )?;

        let bound_producers = build_bound_producers(&runtime_config, &node_config)?;
        let pairing_slots = build_pairing_slots(&runtime_config, &pairing_cardinalities)?;
        let observation_slots =
            build_observation_slots(&runtime_config, &observation_cardinalities)?;

        Ok(Self {
            runtime_config,
            validated_arguments,
            bound_producers,
            pairing_slots,
            pairing_cardinalities,
            observation_slots,
            observation_cardinalities,
            clock_cache: Arc::new(AtomicU64::new(0)),
            endpoint_labels: declared_endpoint_labels(&node_config),
        })
    }

    fn load_runtime_config(path: &str) -> Result<RuntimeConfig> {
        let content = std::fs::read_to_string(path).map_err(|source| Error::LaunchConfigRead {
            path: path.to_string(),
            source,
        })?;
        serde_json5::from_str(&content).map_err(|source| Error::LaunchConfigParse {
            path: path.to_string(),
            source,
        })
    }

    fn validate_fingerprint(peppy_config: &Path, expected: &str) -> Result<()> {
        let actual = RuntimeConfig::generate_peppy_config_fingerprint(peppy_config)?;
        if actual == expected {
            Ok(())
        } else {
            Err(Error::PeppyConfigFingerprintMismatch {
                path: peppy_config.display().to_string(),
                expected: expected.to_string(),
                actual,
            })
        }
    }

    pub fn bound_instance_id(&self) -> &str {
        self.runtime_config.node_instance.instance_id.as_str()
    }

    pub fn bound_core_node(&self) -> &str {
        self.runtime_config.bound_core_node.as_str()
    }

    pub(crate) fn input_arguments(&self) -> &NodeArguments {
        &self.validated_arguments
    }

    pub fn node_name(&self) -> &str {
        self.runtime_config.node_name.as_str()
    }

    pub fn node_tag(&self) -> &str {
        self.runtime_config.node_tag.as_str()
    }

    pub fn messaging_host(&self) -> &str {
        &self.runtime_config.messaging_host
    }

    pub fn messaging_port(&self) -> u16 {
        self.runtime_config.messaging_port
    }

    /// Peer-discovery settings for this node's messaging session.
    pub(crate) fn discovery(&self) -> &config::runtime::DiscoveryConfig {
        &self.runtime_config.discovery
    }

    /// Grace period this node's daemon-liveness watchdog waits, after the
    /// daemon's heartbeat goes silent, before shutting the node down. Resolved
    /// by the daemon from `peppy_config.json5` and shipped in the runtime
    /// config. Read by [`crate::services::daemon_watchdog`].
    pub fn daemon_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.runtime_config.lifecycle.daemon_grace_secs)
    }

    /// Cooperative-shutdown grace window. The daemon waits this long for a
    /// stopping node to exit before force-killing it, and the node runtime
    /// bounds its registered shutdown hooks
    /// ([`crate::runtime::NodeRunner::on_shutdown`]) by the same window.
    /// Resolved by the daemon from `peppy_config.json5` and shipped in the
    /// runtime config; standalone nodes use the built-in default.
    pub fn shutdown_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.runtime_config.lifecycle.shutdown_grace_secs)
    }

    /// The one clock this instance reads, and its role in that clock's
    /// domain. Read by [`crate::clock::for_node`] to install the right
    /// source, and by [`crate::clock::ClockPublisher::for_node`], which hands
    /// a publisher out only to the instance a domain declaration named.
    pub fn clock(&self) -> &config::runtime::ClockBinding {
        &self.runtime_config.node_instance.framework.clock
    }

    /// The instance's shared clock cache. See [`Self::clock_cache`] on the
    /// struct field for why one instance has exactly one.
    pub fn clock_cache(&self) -> &Arc<AtomicU64> {
        &self.clock_cache
    }

    /// The runtime-resolved, immutable, ordered producer set bound to the
    /// consumer slot declared at `link_id`, from the daemon-supplied
    /// `slot_bindings`, cached once at startup for the lifetime of the
    /// node. Serves topic subscribes and service / action calls alike:
    /// every interface kind sharing the slot's `link_id` sees the same set
    /// in the same declaration order, so `.first()` is deterministic. The
    /// set's validated size is the slot's declared cardinality (exactly one
    /// for `one`, at most one for `zero_or_one`, at least one for
    /// `one_or_more`, any size for `zero_or_more`); a producer disconnecting
    /// at runtime never shrinks it. Startup validates every declared slot
    /// into the cache, so a cache miss means the generated code and the
    /// manifest disagree (version skew / stale codegen), a bug rather than a
    /// user error, and panics.
    ///
    /// The generated accessors are cardinality-typed: only `zero_or_more`
    /// slots' `bound_producers()` splice this plain, possibly empty slice
    /// directly. `one` slots go through [`Self::sole_bound_producer`],
    /// `zero_or_one` slots through [`Self::optional_bound_producer`] and
    /// `one_or_more` slots through [`Self::non_empty_bound_producers`];
    /// every generated `subscribe()` passes this slice to
    /// `subscribe_bound_set` regardless of cardinality.
    pub fn bound_producers(&self, link_id: &str) -> &[crate::messaging::ProducerRef] {
        self.bound_producers
            .get(link_id)
            .unwrap_or_else(|| {
                panic!(
                    "consumer slot `{link_id}` has no cached producer set: the generated code \
                     and the manifest disagree (version skew / stale codegen) — regenerate \
                     bindings for this node"
                )
            })
            .as_slice()
    }

    /// The sole producer bound to a `cardinality: "one"` consumer slot.
    /// Startup validated every slot's set size against its declared
    /// cardinality, so exactly one member exists; any other size here means
    /// the generated code and the manifest disagree (version skew / stale
    /// codegen), a bug rather than a user error, and panics just like an
    /// unknown `link_id` in [`Self::bound_producers`].
    ///
    /// Generated `bound_producer()` module functions of `one` slots splice
    /// `node_runner.processor().sole_bound_producer(<link_id>)`.
    pub fn sole_bound_producer(&self, link_id: &str) -> &crate::messaging::ProducerRef {
        match self.bound_producers(link_id) {
            [sole] => sole,
            set => panic!(
                "consumer slot `{link_id}` is bound to {} producers but the generated accessor \
                 expects cardinality `one`: the generated code and the manifest disagree \
                 (version skew / stale codegen); regenerate bindings for this node",
                set.len()
            ),
        }
    }

    /// The producer bound to a `cardinality: "zero_or_one"` consumer slot,
    /// or `None` where the deployment wrote the slot vacant. Startup
    /// validated the slot's set as holding at most one member, so a larger
    /// one here means the generated code and the manifest disagree (version
    /// skew / stale codegen), a bug rather than a user error, and panics
    /// just like an unknown `link_id` in [`Self::bound_producers`].
    ///
    /// Generated `bound_producer()` module functions of `zero_or_one` slots
    /// splice `node_runner.processor().optional_bound_producer(<link_id>)`.
    pub fn optional_bound_producer(&self, link_id: &str) -> Option<&crate::messaging::ProducerRef> {
        match self.bound_producers(link_id) {
            [] => None,
            [sole] => Some(sole),
            set => panic!(
                "consumer slot `{link_id}` is bound to {} producers but the generated accessor \
                 expects cardinality `zero_or_one`: the generated code and the manifest disagree \
                 (version skew / stale codegen); regenerate bindings for this node",
                set.len()
            ),
        }
    }

    /// The producer set bound to a `cardinality: "one_or_more"` consumer
    /// slot, as a [`NonEmptyProducers`](crate::messaging::NonEmptyProducers)
    /// view whose `first()` is infallible. Startup validated the set as
    /// non-empty; an empty set here means the generated code and the
    /// manifest disagree (version skew / stale codegen), a bug rather than
    /// a user error, and panics just like an unknown `link_id` in
    /// [`Self::bound_producers`].
    ///
    /// Generated `bound_producers()` module functions of `one_or_more`
    /// slots splice
    /// `node_runner.processor().non_empty_bound_producers(<link_id>)`.
    pub fn non_empty_bound_producers(
        &self,
        link_id: &str,
    ) -> crate::messaging::NonEmptyProducers<'_> {
        crate::messaging::NonEmptyProducers::new(self.bound_producers(link_id)).unwrap_or_else(
            || {
                panic!(
                    "consumer slot `{link_id}` is bound to an empty set but the generated \
                     accessor expects cardinality `one_or_more`: the generated code and the \
                     manifest disagree (version skew / stale codegen); regenerate bindings for \
                     this node"
                )
            },
        )
    }

    /// Checks that `target` is a member of the bound set of the slot
    /// declared at `link_id`, returning
    /// [`Error::TargetNotBound`](crate::error::Error::TargetNotBound)
    /// otherwise. Generated service `poll` / action `fire_goal` wrappers
    /// call this before anything reaches the wire: `ProducerRef` is plainly
    /// constructible, and an out-of-set instance was never checked by
    /// plan-time binding validation, so letting it through would reopen
    /// undirected `from_any`-style calls. Membership is per slot — a
    /// producer bound to a different slot of the same consumer is rejected
    /// all the same.
    pub fn ensure_target_bound(
        &self,
        link_id: &str,
        target: &crate::messaging::ProducerRef,
    ) -> Result<()> {
        if self.bound_producers(link_id).contains(target) {
            return Ok(());
        }
        Err(Error::TargetNotBound {
            link_id: link_id.to_string(),
            core_node: target.core_node.clone(),
            instance_id: target.instance_id.clone(),
        })
    }

    /// The live watch channel for the pairing slot declared at `link_id`, or
    /// `None` when the manifest declares no such slot. Used by
    /// [`crate::runtime::NodeRunner::peer`] and the generated
    /// `subscribe_peer` seam to observe the slot's pairs; the `peer_update` service uses
    /// the sender side via [`Self::pairing_slot_senders`].
    pub(crate) fn peer_set_watch(&self, link_id: &str) -> Option<watch::Receiver<PeerSetState>> {
        self.pairing_slots.get(link_id).map(|tx| tx.subscribe())
    }

    /// The declared cardinality of the pairing slot at `link_id`, or `None`
    /// when the manifest declares no such slot.
    pub(crate) fn pairing_slot_cardinality(&self, link_id: &str) -> Option<Cardinality> {
        self.pairing_cardinalities.get(link_id).copied()
    }

    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run outside a copy.
    pub fn copy(&self) -> Option<&str> {
        self.runtime_config
            .node_instance
            .copy
            .as_ref()
            .map(|copy| copy.as_str())
    }

    /// Shared handle to all pairing-slot channels, handed to the pre-setup
    /// `peer_update` service listener.
    pub(crate) fn pairing_slot_senders(
        &self,
    ) -> Arc<BTreeMap<String, watch::Sender<PeerSetState>>> {
        Arc::clone(&self.pairing_slots)
    }

    /// The live watch channel for the observer slot declared at `link_id`, or
    /// `None` when the manifest declares no such slot. Used by
    /// [`crate::runtime::NodeRunner::observation_slot`] and the generated
    /// `subscribe_observed` seam to observe the resolved source; the
    /// `observation_update` service uses the sender side via
    /// [`Self::observation_slot_senders`].
    pub(crate) fn observation_slot_watch(
        &self,
        link_id: &str,
    ) -> Option<watch::Receiver<ObservationState>> {
        self.observation_slots.get(link_id).map(|tx| tx.subscribe())
    }

    /// The `cardinality` the manifest declares for the observer slot at
    /// `link_id`, or `None` when it declares no such slot. Types the slot's
    /// accessor: scalar slots (`one`, `zero_or_one`) are read through
    /// [`crate::runtime::NodeRunner::observation_slot`], the multi-member ones
    /// through [`crate::runtime::NodeRunner::observation_slot_set`].
    pub(crate) fn observation_slot_cardinality(&self, link_id: &str) -> Option<Cardinality> {
        self.observation_cardinalities.get(link_id).copied()
    }

    /// Shared handle to all observation-slot channels, handed to the pre-setup
    /// `observation_update` service listener.
    pub(crate) fn observation_slot_senders(
        &self,
    ) -> Arc<BTreeMap<String, watch::Sender<ObservationState>>> {
        Arc::clone(&self.observation_slots)
    }

    /// The labels the manifest declares under `execution.endpoints`, in
    /// label order; empty for a node that serves nothing.
    pub fn declared_endpoints(&self) -> &BTreeSet<EndpointLabel> {
        &self.endpoint_labels
    }
}

/// The endpoint labels of `node_config`, the set the runtime admits
/// announcements for and checks setup against.
fn declared_endpoint_labels(node_config: &NodeConfig) -> BTreeSet<EndpointLabel> {
    node_config.execution.endpoints.keys().cloned().collect()
}

/// Every observer slot declared in `depends_on.pairing_observers`, as an
/// iterator over the manifest entries that both seeders below key on.
fn observer_deps(node_config: &NodeConfig) -> impl Iterator<Item = &PairingObserverDependency> {
    node_config
        .manifest
        .depends_on
        .iter()
        .flat_map(|deps| &deps.pairing_observers)
}

/// Seed one watch channel per **observer** pairing slot declared in
/// `depends_on.pairing_observers`, keyed by slot link_id, each initialized
/// from the boot config's `observation_seeds` entry for that slot: the
/// launch-time member set the daemon stamped at spawn, at sequence zero, so
/// `sources()` answers the plan's membership from the node's first
/// instruction, setup included. Membership changes, generation bumps, and
/// liveness transitions arrive over the `observation_update` service at
/// strictly larger sequences and replace the seed wholesale.
///
/// The startup backstop mirrors the producer-binding rules: a missing seed
/// counts as empty, an empty slot is legal only where the plan could have
/// written one (`zero_or_one` vacant, `zero_or_more` observing nothing), and
/// a seed whose size the declared cardinality does not allow, or that names
/// a slot the manifest does not declare, fails construction as component
/// version skew rather than booting a slot the plan says cannot exist.
///
/// `cardinalities` is [`build_observer_cardinalities`]'s map, so the slots
/// seeded here and the cardinalities their accessors read are one set of
/// link_ids by construction rather than by two walks agreeing.
fn build_observation_slots(
    runtime_config: &RuntimeConfig,
    cardinalities: &BTreeMap<String, Cardinality>,
) -> Result<Arc<BTreeMap<String, watch::Sender<ObservationState>>>> {
    let seeds = &runtime_config.node_instance.observation_seeds;
    if let Some(link_id) = seeds.keys().find(|k| !cardinalities.contains_key(*k)) {
        return Err(Error::ObservationSeedUndeclared {
            link_id: link_id.clone(),
        });
    }

    cardinalities
        .iter()
        .map(|(link_id, cardinality)| {
            let members: Vec<ObservedMemberState> = seeds
                .get(link_id)
                .map(|seeded| seeded.iter().map(ObservedMemberState::from).collect())
                .unwrap_or_default();
            // The wire decoder rejects a repeated observed pairing on a live
            // delivery; the seed path holds the same line, keyed on the whole
            // `ObservedSource`, which is the member identity and carries the
            // `Hash`/`Eq` pinned for exactly this use.
            let mut identities = std::collections::HashSet::with_capacity(members.len());
            for member in &members {
                if !identities.insert(&member.source) {
                    return Err(Error::ObservationSeedDuplicate {
                        link_id: link_id.clone(),
                        core_node: member.source.producer.core_node.clone(),
                        instance_id: member.source.producer.instance_id.clone(),
                        source_link_id: member.source.source_link_id.clone(),
                    });
                }
            }
            if !cardinality.admits(members.len()) {
                return Err(Error::ObservationSeedViolated {
                    link_id: link_id.clone(),
                    cardinality: *cardinality,
                    seeded: members.len(),
                });
            }
            let (tx, _rx) = watch::channel(ObservationState::seeded(members));
            Ok((link_id.clone(), tx))
        })
        .collect::<Result<_>>()
        .map(Arc::new)
}

/// Each observer slot's declared `cardinality`, keyed by link_id. Built once
/// per processor and handed to [`build_observation_slots`], which seeds a
/// channel for exactly these link_ids.
fn build_observer_cardinalities(node_config: &NodeConfig) -> BTreeMap<String, Cardinality> {
    observer_deps(node_config)
        .map(|dep| (dep.link_id.clone(), dep.cardinality))
        .collect()
}

/// One watch channel per declared `depends_on.pairings` slot, seeded with the
/// pairs the boot config's `pairing_slots` entry carries for it. A
/// daemon-spawned instance seeds empty and its pairs arrive over
/// `peer_update` once it commits to Running, so a seed may hold fewer pairs
/// than the slot's floor. A scalar slot's seed holds at most one: several
/// pairs are [`Error::PairingSeedNotScalar`], since the generated code reads
/// the slot through `peer()`, which has one peer to answer. A seed naming an
/// undeclared slot or repeating one pair is refused the way
/// [`build_observation_slots`] refuses the same two.
///
/// An observer slot is not a pairing slot: it holds no peer set and receives
/// no `peer_update`. Its source pins arrive over `observation_update` into a
/// separate observation-slot channel.
fn build_pairing_slots(
    runtime_config: &RuntimeConfig,
    cardinalities: &BTreeMap<String, Cardinality>,
) -> Result<Arc<BTreeMap<String, watch::Sender<PeerSetState>>>> {
    let seeds = &runtime_config.node_instance.pairing_slots;
    if let Some(link_id) = seeds.keys().find(|k| !cardinalities.contains_key(*k)) {
        return Err(Error::PairingSeedUndeclared {
            link_id: link_id.clone(),
        });
    }

    cardinalities
        .iter()
        .map(|(link_id, cardinality)| {
            let seeded: Vec<PeerMember> = seeds
                .get(link_id)
                .map(|pairs| pairs.iter().map(PeerMember::from).collect())
                .unwrap_or_default();
            // A pair is one peer to this slot, and a slot holds each peer
            // once. The identity is the peer triple, which is what the wire
            // decoder keys its own duplicate refusal on.
            let mut identities = std::collections::HashSet::with_capacity(seeded.len());
            for member in &seeded {
                if !identities.insert(&member.info) {
                    return Err(Error::PairingSeedDuplicate {
                        link_id: link_id.clone(),
                        core_node: member.info.producer.core_node.clone(),
                        instance_id: member.info.producer.instance_id.clone(),
                        peer_link_id: member.info.peer_link_id.clone(),
                    });
                }
            }
            if !cardinality.admits_seed(seeded.len()) {
                return Err(Error::PairingSeedNotScalar {
                    link_id: link_id.clone(),
                    cardinality: cardinality.as_str().to_string(),
                    count: seeded.len(),
                });
            }
            let (tx, _rx) = watch::channel(PeerSetState::seeded(seeded));
            Ok((link_id.clone(), tx))
        })
        .collect::<Result<BTreeMap<_, _>>>()
        .map(Arc::new)
}

/// Each pairing slot's declared `cardinality`, from the manifest.
fn build_pairing_cardinalities(node_config: &NodeConfig) -> BTreeMap<String, Cardinality> {
    node_config
        .manifest
        .depends_on
        .as_ref()
        .map(|deps| {
            deps.pairings
                .iter()
                .map(|dep| (dep.link_id.clone(), dep.cardinality))
                .collect()
        })
        .unwrap_or_default()
}

/// Pre-resolve the ordered bound producer set for every `link_id` declared
/// in the consumer manifest's `depends_on`, from the daemon-supplied
/// `slot_bindings`, enforcing each slot's declared cardinality:
///
/// - `one`: the slot's entry must hold exactly one producer; a missing
///   entry is [`Error::SlotUnbound`], a wrong-sized one is
///   [`Error::SlotCardinalityViolated`].
/// - `zero_or_one`: the entry must hold at most one producer. A slot the
///   deployment wrote vacant arrives as an explicit empty entry, so a
///   missing one is [`Error::SlotUnbound`] like a `one` slot's.
/// - `one_or_more`: the entry must hold at least one producer.
/// - `zero_or_more`: a missing entry and an empty entry are both the valid
///   empty set.
///
/// The launcher validator enforces the same rules at plan time, so a
/// violation here means version skew or a hand-edited boot config. Called
/// once during [`Processor::new_daemon`] / [`Processor::new_standalone`] so
/// the per-link_id accessor is a borrow into a stable cache. Member order
/// is preserved verbatim from the boot config (application declaration
/// order).
fn build_bound_producers(
    runtime_config: &RuntimeConfig,
    node_config: &NodeConfig,
) -> Result<BTreeMap<String, config::runtime::BoundProducers>> {
    let mut out = BTreeMap::new();
    if let Some(deps) = node_config.manifest.depends_on.as_ref() {
        let slot_bindings = &runtime_config.node_instance.slot_bindings;
        let node_slots = deps.nodes.iter().map(|dep| (&dep.link_id, dep.cardinality));
        let contract_slots = deps
            .contracts
            .iter()
            .map(|dep| (&dep.link_id, dep.cardinality));
        for (link_id, cardinality) in node_slots.chain(contract_slots) {
            let bound = match slot_bindings.get(link_id) {
                Some(bound) => bound.clone(),
                None if cardinality.allows_empty() => config::runtime::BoundProducers::default(),
                None => {
                    return Err(Error::SlotUnbound {
                        link_id: link_id.clone(),
                        cardinality,
                    });
                }
            };
            if !cardinality.admits(bound.len()) {
                return Err(Error::SlotCardinalityViolated {
                    link_id: link_id.clone(),
                    cardinality,
                    bound: bound.len(),
                });
            }
            out.insert(link_id.clone(), bound);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{PEPPYGEN_OUTPUT_PATH, Processor};
    use crate::runtime::builder::StandaloneConfig;
    use config::node::{Cardinality, TypeToken};
    use config::{
        AnyType, ParameterSchema, ParameterSpec, runtime::RuntimeConfig, validate_node_arguments,
    };
    use std::{collections::BTreeMap, path::Path};
    use tempfile::TempDir;

    #[test]
    fn loads_runtime_config_from_path() {
        let bound_core_node = "epic-whale-6789";
        let bound_node_name = "uvc_camera";
        let bound_instance_id = "camera_front";

        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Create a peppy config file with type specifications matching runtime parameters
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
            },
            execution: {
                language: "rust",
                parameters: {
                    exposure: "f32",
                    flags: {
                        $type: "array",
                        $items: "string"
                    },
                    nested: {
                        $type: "object",
                        enabled: "bool",
                        gain: "i64"
                    },
                    mode: "string"
                },
                run_cmd: ["./target/debug/uvc_camera"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "$INSTANCE_ID",
                arguments: {
                    exposure: 0.25,
                    flags: ["hdr", "stabilized"],
                    nested: { enabled: true, gain: 10 },
                    mode: "auto"
                }
            },
            node_name: "$NODE_NAME",
            node_tag: "v1",
            bound_core_node: "$CORE_NODE"
        }"#;

        let populated_config = json5_config
            .replace("$INSTANCE_ID", bound_instance_id)
            .replace("$NODE_NAME", bound_node_name)
            .replace("$CORE_NODE", bound_core_node);

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&populated_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let runtime_processor = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path
                .to_str()
                .expect("runtime config path should be valid UTF-8"),
        )
        .expect("runtime processor should load config from path");

        let mut expected_parameters: BTreeMap<String, AnyType> = BTreeMap::new();
        expected_parameters.insert("exposure".into(), AnyType::Float(0.25));
        expected_parameters.insert(
            "flags".into(),
            AnyType::Array(vec![
                AnyType::String("hdr".into()),
                AnyType::String("stabilized".into()),
            ]),
        );
        expected_parameters.insert(
            "nested".into(),
            AnyType::Object(BTreeMap::from([
                ("enabled".to_string(), AnyType::Bool(true)),
                ("gain".to_string(), AnyType::Int(10)),
            ])),
        );
        expected_parameters.insert("mode".into(), AnyType::String("auto".into()));

        assert_eq!(runtime_processor.bound_instance_id(), bound_instance_id);
        assert_eq!(runtime_processor.bound_core_node(), bound_core_node);
        assert_eq!(runtime_processor.node_name(), bound_node_name);
        assert_eq!(
            serde_json::to_value(runtime_processor.input_arguments()).unwrap(),
            serde_json::to_value(&expected_parameters).unwrap(),
        );
    }

    #[test]
    fn fails_when_codegen_fingerprint_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_wrong_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42 }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected fingerprint mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("fingerprint mismatch"),
            "expected fingerprint mismatch error, got: {err_string}"
        );
    }

    #[test]
    fn fails_when_runtime_parameter_missing_in_compiled_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config only has 'value' parameter
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config has 'value' AND 'extra_param' - but 'extra_param' is not in compiled
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42, extra_param: "unexpected" }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected missing parameter error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("unknown parameter") && err_string.contains("extra_param"),
            "expected unknown parameter error for 'extra_param', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_parameter_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects 'value' to be i64
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides 'value' as a string instead of i64
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: "not_an_integer" }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("value"),
            "expected type mismatch error for 'value', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_nested_parameter_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects nested object with specific types
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    config: {
                        $type: "object",
                        enabled: "bool",
                        threshold: "f64"
                    }
                },
                run_cmd: ["./target/debug/test_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides 'enabled' as string instead of bool
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { config: { enabled: "yes", threshold: 0.5 } }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("config.enabled"),
            "expected type mismatch error for 'config.enabled', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_array_item_type_mismatch() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        // Compiled config expects array of strings
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    tags: {
                        $type: "array",
                        $items: "string"
                    }
                },
                run_cmd: ["./target/debug/test_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );

        // Runtime config provides array with mixed types (string and int)
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { tags: ["valid", 123, "also_valid"] }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#
        .to_string();

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected type mismatch error");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("type mismatch") && err_string.contains("tags[1]"),
            "expected type mismatch error for 'tags[1]', got: {err_string}"
        );
    }

    #[test]
    fn fails_when_codegen_fingerprint_missing() {
        use crate::error::Error;

        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/test_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");
        // Note: intentionally NOT creating the fingerprint file

        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "test_instance",
                arguments: { value: 42 }
            },
            node_name: "test_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#;

        let runtime_config: RuntimeConfig =
            serde_json5::from_str(json5_config).expect("runtime config should parse");

        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        let Err(err) = Processor::new_daemon_from_path(
            &peppy_config_path,
            runtime_config_path.to_str().unwrap(),
        ) else {
            panic!("expected codegen fingerprint read error");
        };
        assert!(
            matches!(err, Error::CodegenFingerprintRead { .. }),
            "expected CodegenFingerprintRead error, got: {err:?}"
        );
    }

    /// A manifest with one parameter and one contract slot, as a node that
    /// builds its own manifest would hold it: never written to disk.
    fn in_memory_manifest() -> config::node::NodeConfig {
        serde_json5::from_str(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "built_in_server",
                    tag: "v1",
                    depends_on: {
                        contracts: [{ name: "rgb_camera", tag: "v1", link_id: "camera" }],
                    },
                },
                interfaces: {
                    services: { consumes: [{ link_id: "camera", name: "video_stream_info" }] },
                },
                execution: {
                    language: "rust",
                    parameters: { port: { $type: "u16", $default: 8900 } },
                },
            }"#,
        )
        .expect("the manifest parses")
    }

    #[test]
    fn daemon_mode_from_an_in_memory_manifest_needs_no_file_and_no_fingerprint() {
        let runtime_config = RuntimeConfig::new(
            "127.0.0.1",
            7448,
            config::runtime::NodeInstanceConfig {
                arguments: BTreeMap::from([("port".to_string(), AnyType::Int(9000))]),
                slot_bindings: BTreeMap::from([(
                    "camera".to_string(),
                    config::runtime::ProducerRef::new("epic-whale-6789", "the_camera").into(),
                )]),
                ..config::runtime::NodeInstanceConfig::new(
                    config::runtime::Name::new("mcp").expect("valid instance id"),
                )
            },
            "built_in_server",
            "v1",
            "epic-whale-6789",
        )
        .expect("runtime config builds");

        let processor = Processor::daemon_from_manifest(runtime_config, in_memory_manifest())
            .expect("the manifest and the launch config agree");

        assert_eq!(processor.node_name(), "built_in_server");
        assert_eq!(processor.bound_instance_id(), "mcp");
        assert_eq!(processor.bound_core_node(), "epic-whale-6789");
        let arguments = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(arguments["port"], serde_json::json!(9000));
        assert_eq!(
            processor.sole_bound_producer("camera"),
            &config::runtime::ProducerRef::new("epic-whale-6789", "the_camera")
        );
    }

    #[test]
    fn daemon_mode_from_an_in_memory_manifest_still_validates_the_launch_arguments() {
        let runtime_config = RuntimeConfig::new(
            "127.0.0.1",
            7448,
            config::runtime::NodeInstanceConfig {
                arguments: BTreeMap::from([(
                    "port".to_string(),
                    AnyType::String("not a port".to_string()),
                )]),
                slot_bindings: BTreeMap::from([(
                    "camera".to_string(),
                    config::runtime::ProducerRef::new("epic-whale-6789", "the_camera").into(),
                )]),
                ..config::runtime::NodeInstanceConfig::new(
                    config::runtime::Name::new("mcp").expect("valid instance id"),
                )
            },
            "built_in_server",
            "v1",
            "epic-whale-6789",
        )
        .expect("runtime config builds");

        let error = Processor::daemon_from_manifest(runtime_config, in_memory_manifest())
            .err()
            .expect("a mistyped argument is refused");
        assert!(
            error.to_string().contains("port"),
            "the refusal names the argument: {error}"
        );
    }

    #[test]
    fn standalone_mode_from_an_in_memory_manifest_reads_it_like_the_file() {
        let config =
            StandaloneConfig::new().with_bound_producer("camera", "standalone-core", "the_camera");
        let processor = Processor::standalone_from_manifest(in_memory_manifest(), &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "built_in_server");
        assert_eq!(processor.bound_instance_id(), "standalone");
        let arguments = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(
            arguments["port"],
            serde_json::json!(8900),
            "the parameter default fills an omitted argument"
        );
        assert_eq!(
            processor.sole_bound_producer("camera"),
            &config::runtime::ProducerRef::new("standalone-core", "the_camera")
        );
    }

    #[test]
    fn standalone_mode_uses_manifest_node_name() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "my_node");
        assert_eq!(processor.bound_instance_id(), "standalone");
        assert_eq!(processor.bound_core_node(), "standalone-core");
        assert_eq!(processor.messaging_host(), "127.0.0.1");
        assert_eq!(processor.messaging_port(), 7448);
    }

    #[test]
    fn standalone_mode_resolves_the_clock_like_a_launch_would() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let wall = Processor::new_standalone(&peppy_config_path, &StandaloneConfig::new())
            .expect("should create processor");
        assert!(wall.clock().is_wall(), "wall time is the default");

        let domain = config::runtime::ClockDomainId::new(
            config::runtime::Name::new("robot").expect("valid name"),
            config::runtime::CoreNodeName::new("cn-sim").expect("valid core node name"),
            config::runtime::ClockIncarnation::try_from(3).expect("non-zero"),
        );
        let sim = Processor::new_standalone(
            &peppy_config_path,
            &StandaloneConfig::new()
                .with_clock(config::runtime::ClockBinding::publisher(domain.clone())),
        )
        .expect("should create processor");
        assert!(
            sim.clock().is_publisher(),
            "with_clock must land in the resolved framework block"
        );
        assert_eq!(sim.clock().domain(), Some(&domain));
    }

    #[test]
    fn standalone_mode_with_custom_config() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new()
            .with_node_name("custom_name")
            .with_instance_id("custom_instance")
            .with_messaging("192.168.1.100", 9999);

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        assert_eq!(processor.node_name(), "custom_name");
        assert_eq!(processor.bound_instance_id(), "custom_instance");
        assert_eq!(processor.messaging_host(), "192.168.1.100");
        assert_eq!(processor.messaging_port(), 9999);
    }

    #[test]
    fn standalone_mode_with_json5_parameters() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "value": 42 }));

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("value"), Some(&serde_json::json!(42)));
    }

    #[test]
    fn standalone_mode_with_typed_parameters() {
        use serde::Serialize;

        #[derive(Serialize)]
        struct TestParams {
            threshold: f64,
            enabled: bool,
        }

        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { threshold: "f64", enabled: "bool" }, run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let params = TestParams {
            threshold: 0.75,
            enabled: true,
        };
        let config = StandaloneConfig::new().with_parameters(&params);

        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("threshold"), Some(&serde_json::json!(0.75)));
        assert_eq!(args_json.get("enabled"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn standalone_mode_peer_pin_is_retained_for_late_subscriber() {
        // `with_peer_pin` seeds the slot before any watch receiver exists
        // (the first `peer(...)` subscribes later); the seed must land in the
        // receiver-less channel rather than being discarded.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "my_node",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    ],
                },
            },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new().with_peer_pin("arm", "core_a", "arm_1", "controller");
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let watch_rx = processor
            .peer_set_watch("arm")
            .expect("declared slot should have a channel");
        let state = watch_rx.borrow();
        let [member] = state.members.as_slice() else {
            panic!("the seeded pair should survive until the first subscriber");
        };
        assert_eq!(member.info.producer.core_node, "core_a");
        assert_eq!(member.info.producer.instance_id, "arm_1");
        assert_eq!(member.info.peer_link_id, "controller");
        assert_eq!(member.copy, None);
    }

    /// Manifest fixture with one scalar and one multi pairing slot.
    const PAIRING_SLOTS_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "my_node",
            tag: "v1",
            depends_on: {
                pairings: [
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
                    { name: "arm_link", tag: "v1", role: "controller", link_id: "limbs", cardinality: "zero_or_more" },
                ],
            },
        },
        execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
    }"#;

    #[test]
    fn standalone_mode_seeds_a_multi_slot_with_every_pin_and_its_copy() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, PAIRING_SLOTS_CONFIG)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new()
            .with_peer_pin_in_copy("limbs", "core_a", "left_1", "engine", "robot_a")
            .with_peer_pin("limbs", "core_a", "left_2", "engine");
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("should create processor");

        let watch_rx = processor
            .peer_set_watch("limbs")
            .expect("declared slot should have a channel");
        let state = watch_rx.borrow();
        let copies: Vec<(String, Option<String>)> = state
            .members
            .iter()
            .map(|member| {
                (
                    member.info.producer.instance_id.clone(),
                    member.copy.clone(),
                )
            })
            .collect();
        assert_eq!(
            copies,
            vec![
                ("left_1".to_string(), Some("robot_a".to_string())),
                ("left_2".to_string(), None),
            ]
        );
        assert!(
            processor
                .peer_set_watch("arm")
                .expect("declared slot should have a channel")
                .borrow()
                .members
                .is_empty(),
            "an unseeded slot boots unpaired"
        );
    }

    #[test]
    fn standalone_mode_refuses_two_pins_on_a_scalar_slot() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, PAIRING_SLOTS_CONFIG)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new()
            .with_peer_pin("arm", "core_a", "arm_1", "controller")
            .with_peer_pin("arm", "core_a", "arm_2", "controller");
        let Err(err) = Processor::new_standalone(&peppy_config_path, &config) else {
            panic!("a scalar slot holds one pair");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::PairingSeedNotScalar { link_id, count: 2, .. } if link_id == "arm"
            ),
            "got: {err:?}"
        );
        assert!(err.to_string().contains("one_or_more"), "{err}");
    }

    #[test]
    fn standalone_mode_refuses_a_copy_that_is_not_a_name() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, PAIRING_SLOTS_CONFIG)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new()
            .with_peer_pin_in_copy("limbs", "core_a", "left_1", "engine", "robot a");
        let Err(err) = Processor::new_standalone(&peppy_config_path, &config) else {
            panic!("a copy is a name");
        };
        assert!(
            matches!(&err, crate::error::Error::InvalidCopyName { copy, .. } if copy == "robot a"),
            "got: {err:?}"
        );
    }

    #[test]
    fn standalone_mode_fails_when_required_parameters_missing() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { value: "i64" }, run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        // No parameters provided — should fail immediately
        let config = StandaloneConfig::new();
        let result = Processor::new_standalone(&peppy_config_path, &config);

        let Err(err) = result else {
            panic!("expected error when required parameters are missing");
        };
        assert!(
            matches!(err, crate::error::Error::NodeArgumentsValidation(_)),
            "expected NodeArgumentsValidation error, got: {err:?}"
        );
        let err_string = err.to_string();
        assert!(
            err_string.contains("value"),
            "error should mention missing parameter 'value', got: {err_string}"
        );
    }

    #[test]
    fn standalone_mode_fails_when_some_parameters_missing() {
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", parameters: { threshold: "f64", enabled: "bool", name: "string" }, run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        // Only provide one of three required parameters
        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "threshold": 0.5 }));
        let result = Processor::new_standalone(&peppy_config_path, &config);

        let Err(err) = result else {
            panic!("expected error when some required parameters are missing");
        };
        let err_string = err.to_string();
        assert!(
            err_string.contains("enabled") && err_string.contains("name"),
            "error should mention missing parameters 'enabled' and 'name', got: {err_string}"
        );
        // The provided parameter should NOT be mentioned
        assert!(
            !err_string.contains("threshold"),
            "error should not mention provided parameter 'threshold', got: {err_string}"
        );
    }

    #[test]
    fn standalone_mode_fills_defaults_for_omitted_parameters() {
        // Partial config: user omits `frame_rate`, runtime fills it from $default.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    name: "string",
                    frame_rate: { $type: "u16", $default: 30 }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config =
            StandaloneConfig::new().with_parameters_json(serde_json::json!({ "name": "front" }));
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("standalone with partial parameters should succeed");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(args_json.get("name"), Some(&serde_json::json!("front")));
        assert_eq!(args_json.get("frame_rate"), Some(&serde_json::json!(30)));
    }

    #[test]
    fn standalone_mode_synthesizes_fully_defaulted_group() {
        // The whole `device` group is omitted; every leaf has a default,
        // so the runtime synthesizes the group from defaults.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        path: { $type: "string", $default: "/dev/video0" },
                        auto_detect: { $type: "bool", $default: true }
                    }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let processor = Processor::new_standalone(&peppy_config_path, &config)
            .expect("fully-defaulted group should be synthesized");

        let args_json = serde_json::to_value(processor.input_arguments()).unwrap();
        assert_eq!(
            args_json.get("device"),
            Some(&serde_json::json!({ "path": "/dev/video0", "auto_detect": true }))
        );
    }

    #[test]
    fn standalone_mode_partial_default_group_reports_missing_leaf() {
        // `device.serial` has no default (USB serials vary per unit); the
        // error must name that leaf by full dot-path so users know what to
        // supply.
        let temp_dir = TempDir::new().expect("temp dir should be created");

        let peppy_config_path = temp_dir.path().join("peppy.json5");
        let peppy_config_content = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        path: { $type: "string", $default: "/dev/video0" },
                        serial: "string"
                    }
                },
                run_cmd: ["./target/debug/my_node"]
            },
        }"#;
        std::fs::write(&peppy_config_path, peppy_config_content)
            .expect("peppy config should be written");

        let config = StandaloneConfig::new();
        let result = Processor::new_standalone(&peppy_config_path, &config);
        let err = result.err().expect("missing required leaf should error");
        let msg = err.to_string();
        assert!(
            msg.contains("device.serial"),
            "error should name the missing leaf path, got: {msg}"
        );
    }

    #[test]
    fn validated_arguments_cannot_be_serialized_back_to_raw() {
        // NodeArguments derives Serialize but does not expose the inner
        // data — the only way to consume it is through
        // deserialize_parameters, which parses into a typed struct.
        let arguments = BTreeMap::from([("x".to_string(), AnyType::Int(1))]);
        let schema = ParameterSchema::from([(
            "x".to_string(),
            ParameterSpec::Primitive {
                kind: TypeToken::I64,
                default: None,
            },
        )]);
        let validated =
            validate_node_arguments(arguments, &schema).expect("validation should pass");

        // We can serialize (for deserialize_parameters) but cannot access
        // the inner map directly — this is a compile-time guarantee.
        let json = serde_json::to_value(&validated).expect("should serialize");
        assert_eq!(json.get("x"), Some(&serde_json::json!(1)));
    }

    /// Manifest fixture declaring one default-cardinality consumer slot
    /// (`main`), shared by the unbound-slot startup tests below.
    const CONSUMER_PEPPY_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "consumer_node",
            tag: "v1",
            depends_on: {
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }
        },
        execution: { language: "rust", run_cmd: ["./target/debug/consumer_node"] },
    }"#;

    /// Manifest fixture with one slot of each cardinality: `main` (one,
    /// omitted), `wrist_camera` (zero_or_one), `arms` (one_or_more),
    /// `spare_cameras` (zero_or_more).
    const MULTI_SLOT_PEPPY_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "consumer_node",
            tag: "v1",
            depends_on: {
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "wrist_camera", cardinality: "zero_or_one" },
                    { name: "robot_arm", tag: "v1", link_id: "arms", cardinality: "one_or_more" }
                ],
                contracts: [
                    { name: "uvc_camera", tag: "v1", link_id: "spare_cameras", cardinality: "zero_or_more" }
                ]
            }
        },
        execution: { language: "rust", run_cmd: ["./target/debug/consumer_node"] },
    }"#;

    /// Builds a daemon-mode processor from a manifest string and the
    /// `slot_bindings` JSON5 block of the boot config (pass `None` to omit
    /// the field entirely).
    fn daemon_processor_with_bindings(
        peppy_config: &str,
        slot_bindings_json5: Option<&str>,
    ) -> Result<Processor, crate::error::Error> {
        let instance_block = match slot_bindings_json5 {
            Some(bindings) => {
                format!("{{ instance_id: \"consumer_1\", slot_bindings: {bindings} }}")
            }
            None => "{ instance_id: \"consumer_1\" }".to_string(),
        };
        daemon_processor_with_instance_block(peppy_config, &instance_block)
    }

    fn daemon_processor_with_instance_block(
        peppy_config: &str,
        instance_block: &str,
    ) -> Result<Processor, crate::error::Error> {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, peppy_config).expect("peppy config should be written");
        config::fingerprint::create_codegen_fingerprint(
            &peppy_config_path,
            Path::new(PEPPYGEN_OUTPUT_PATH),
        );
        let json5_config = format!(
            r#"{{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: {instance_block},
                node_name: "consumer_node",
                node_tag: "v1",
                bound_core_node: "core-1234"
            }}"#
        );
        let runtime_config: RuntimeConfig =
            serde_json5::from_str(&json5_config).expect("runtime config should parse");
        let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
        runtime_config
            .save_json5_launch_config(&runtime_config_path)
            .expect("runtime config should be saved");

        Processor::new_daemon_from_path(&peppy_config_path, runtime_config_path.to_str().unwrap())
    }

    fn instance_ids(producers: &[config::runtime::ProducerRef]) -> Vec<&str> {
        producers.iter().map(|p| p.instance_id.as_str()).collect()
    }

    /// The runtime backstop of the launch-time rule "every declared `one` /
    /// `one_or_more` slot must be bound": a boot config missing such a
    /// slot's binding entry fails processor construction, not lazily at
    /// first call.
    #[test]
    fn fails_at_startup_when_declared_slot_has_no_binding() {
        let Err(err) = daemon_processor_with_bindings(CONSUMER_PEPPY_CONFIG, None) else {
            panic!("expected unbound-slot startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::SlotUnbound { link_id, cardinality }
                    if link_id == "main" && *cardinality == Cardinality::One
            ),
            "expected SlotUnbound for `main`, got: {err}"
        );
    }

    /// Daemon processor over [`MULTI_SLOT_PEPPY_CONFIG`] with the standard
    /// bindings: `main` (one) -> camera_1, `wrist_camera` (zero_or_one) ->
    /// camera_2, `arms` (one_or_more) -> [right_arm, left_arm],
    /// `spare_cameras` (zero_or_more) -> [].
    fn multi_slot_processor() -> Processor {
        daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    wrist_camera: [{ core_node: "core-1234", instance_id: "camera_2" }],
                    arms: [
                        { core_node: "core-1234", instance_id: "right_arm" },
                        { core_node: "core-1234", instance_id: "left_arm" }
                    ],
                    spare_cameras: []
                }"#,
            ),
        )
        .expect("valid bindings should construct")
    }

    /// The same fixture with `wrist_camera` resolved to the explicit empty
    /// set a launcher writes for a slot a deployment declared vacant.
    fn multi_slot_processor_with_vacant_wrist_camera() -> Processor {
        daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    wrist_camera: [],
                    arms: [{ core_node: "core-1234", instance_id: "right_arm" }],
                    spare_cameras: []
                }"#,
            ),
        )
        .expect("a vacant zero_or_one slot is a valid empty set")
    }

    /// Happy path: each slot's bound set reaches the startup cache with
    /// member order preserved, and `ensure_target_bound` enforces per-slot
    /// membership.
    #[test]
    fn daemon_boot_config_bindings_reach_bound_producer_cache() {
        let processor = multi_slot_processor();

        assert_eq!(
            instance_ids(processor.bound_producers("main")),
            ["camera_1"]
        );
        assert_eq!(
            instance_ids(processor.bound_producers("arms")),
            ["right_arm", "left_arm"],
            "binding declaration order must be preserved, not sorted"
        );
        assert!(processor.bound_producers("spare_cameras").is_empty());

        // Membership is per slot: a producer bound to a different slot of
        // the same consumer is rejected all the same.
        let right_arm = config::runtime::ProducerRef::new("core-1234", "right_arm");
        processor
            .ensure_target_bound("arms", &right_arm)
            .expect("bound member must pass the membership check");
        let err = processor
            .ensure_target_bound("main", &right_arm)
            .expect_err("cross-slot target must fail the membership check");
        assert!(
            matches!(
                &err,
                crate::error::Error::TargetNotBound { link_id, instance_id, .. }
                    if link_id == "main" && instance_id == "right_arm"
            ),
            "expected TargetNotBound, got: {err}"
        );
    }

    /// The cardinality-typed accessors expose exactly the guarantee startup
    /// validation established: `sole_bound_producer` returns a `one` slot's
    /// single member directly, `optional_bound_producer` a `zero_or_one`
    /// slot's member as an `Option`, and `non_empty_bound_producers` a
    /// `one_or_more` slot's ordered set with an infallible `first()`.
    #[test]
    fn cardinality_typed_accessors_return_the_validated_shapes() {
        let processor = multi_slot_processor();

        assert_eq!(
            processor.sole_bound_producer("main").instance_id,
            "camera_1"
        );

        assert_eq!(
            processor
                .optional_bound_producer("wrist_camera")
                .map(|producer| producer.instance_id.as_str()),
            Some("camera_2")
        );
        assert_eq!(
            multi_slot_processor_with_vacant_wrist_camera().optional_bound_producer("wrist_camera"),
            None,
            "a vacant zero_or_one slot resolves to an empty set, which reads as `None`"
        );

        let arms = processor.non_empty_bound_producers("arms");
        assert_eq!(
            arms.first().instance_id,
            "right_arm",
            "first() follows binding declaration order"
        );
        assert_eq!(instance_ids(arms.as_slice()), ["right_arm", "left_arm"]);
        assert_eq!(
            arms.as_slice(),
            processor.bound_producers("arms"),
            "the typed view exposes the same cached set as the plain slice"
        );
    }

    /// A typed accessor whose guarantee the cached set does not meet is
    /// codegen / runtime skew (the accessor and the startup validation come
    /// from the same manifest), so it panics like an unknown `link_id`
    /// rather than returning an error the caller could mishandle.
    #[test]
    #[should_panic(expected = "expects cardinality `one`")]
    fn sole_bound_producer_panics_on_a_multi_member_set() {
        let _ = multi_slot_processor().sole_bound_producer("arms");
    }

    /// See [`sole_bound_producer_panics_on_a_multi_member_set`]: the
    /// non-empty view refuses an empty `zero_or_more` set the same way.
    #[test]
    #[should_panic(expected = "expects cardinality `one_or_more`")]
    fn non_empty_bound_producers_panics_on_an_empty_set() {
        let _ = multi_slot_processor().non_empty_bound_producers("spare_cameras");
    }

    /// See [`sole_bound_producer_panics_on_a_multi_member_set`]: the scalar
    /// `Option` accessor refuses a set of two the same way. An empty set is
    /// this accessor's `None`, so only the ceiling can be violated.
    #[test]
    #[should_panic(expected = "expects cardinality `zero_or_one`")]
    fn optional_bound_producer_panics_on_a_multi_member_set() {
        let _ = multi_slot_processor().optional_bound_producer("arms");
    }

    /// A boot config carrying the removed pre-cardinality single-producer
    /// object shape must fail to parse with the version-skew message: the
    /// daemon, CLI, generated bindings, and node runtime ship together.
    #[test]
    fn boot_config_with_pre_cardinality_object_binding_fails_to_parse() {
        let json5_config = r#"{
            messaging_host: "127.0.0.1",
            messaging_port: 7448,
            node_instance: {
                instance_id: "consumer_1",
                slot_bindings: {
                    main: { core_node: "core-1234", instance_id: "camera_1" }
                }
            },
            node_name: "consumer_node",
            node_tag: "v1",
            bound_core_node: "core-1234"
        }"#;
        let err = serde_json5::from_str::<RuntimeConfig>(json5_config)
            .expect_err("object-valued slot binding must be a hard parse error");
        assert!(
            err.to_string().contains("upgraded together"),
            "parse error should name the version skew, got: {err}"
        );
    }

    /// The cardinality size rules are re-checked at startup: a `one` slot
    /// with two producers and a `one_or_more` slot with an empty set are
    /// both rejected even though the boot config parses.
    #[test]
    fn fails_at_startup_when_bound_set_size_violates_cardinality() {
        let two_on_a_one_slot = daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [
                        { core_node: "core-1234", instance_id: "camera_1" },
                        { core_node: "core-1234", instance_id: "camera_2" }
                    ],
                    wrist_camera: [],
                    arms: [{ core_node: "core-1234", instance_id: "right_arm" }],
                    spare_cameras: []
                }"#,
            ),
        );
        let Err(two_on_a_one_slot) = two_on_a_one_slot else {
            panic!("two producers on a `one` slot must fail startup");
        };
        assert!(
            matches!(
                &two_on_a_one_slot,
                crate::error::Error::SlotCardinalityViolated { link_id, cardinality, bound }
                    if link_id == "main" && *cardinality == Cardinality::One && *bound == 2
            ),
            "expected SlotCardinalityViolated for `main`, got: {two_on_a_one_slot}"
        );

        let empty_one_or_more = daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    wrist_camera: [],
                    arms: [],
                    spare_cameras: []
                }"#,
            ),
        );
        let Err(empty_one_or_more) = empty_one_or_more else {
            panic!("an empty set on a `one_or_more` slot must fail startup");
        };
        assert!(
            matches!(
                &empty_one_or_more,
                crate::error::Error::SlotCardinalityViolated { link_id, cardinality, bound }
                    if link_id == "arms" && *cardinality == Cardinality::OneOrMore && *bound == 0
            ),
            "expected SlotCardinalityViolated for `arms`, got: {empty_one_or_more}"
        );

        let two_on_a_zero_or_one_slot = daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    wrist_camera: [
                        { core_node: "core-1234", instance_id: "camera_2" },
                        { core_node: "core-1234", instance_id: "camera_3" }
                    ],
                    arms: [{ core_node: "core-1234", instance_id: "right_arm" }],
                    spare_cameras: []
                }"#,
            ),
        );
        let Err(two_on_a_zero_or_one_slot) = two_on_a_zero_or_one_slot else {
            panic!("two producers on a `zero_or_one` slot must fail startup");
        };
        assert!(
            matches!(
                &two_on_a_zero_or_one_slot,
                crate::error::Error::SlotCardinalityViolated { link_id, cardinality, bound }
                    if link_id == "wrist_camera"
                        && *cardinality == Cardinality::ZeroOrOne
                        && *bound == 2
            ),
            "expected SlotCardinalityViolated for `wrist_camera`, got: \
             {two_on_a_zero_or_one_slot}"
        );
    }

    /// A `zero_or_one` slot has no silent empty state: its entry must be in
    /// `slot_bindings` even when it is empty, so a boot config that omits it
    /// fails startup exactly as a `one` slot's would. This is what keeps
    /// "the deployment declared this slot vacant" distinguishable from "the
    /// component that wrote this boot config forgot the slot".
    #[test]
    fn zero_or_one_slot_still_needs_its_binding_entry() {
        let Err(err) = daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    arms: [{ core_node: "core-1234", instance_id: "right_arm" }],
                    spare_cameras: []
                }"#,
            ),
        ) else {
            panic!("a missing zero_or_one entry must fail startup");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::SlotUnbound { link_id, cardinality }
                    if link_id == "wrist_camera" && *cardinality == Cardinality::ZeroOrOne
            ),
            "expected SlotUnbound for `wrist_camera`, got: {err}"
        );
    }

    /// A `zero_or_more` slot may be left out of `slot_bindings` entirely;
    /// it resolves to the same empty set as an explicit empty array.
    #[test]
    fn zero_or_more_slot_tolerates_missing_binding_entry() {
        let processor = daemon_processor_with_bindings(
            MULTI_SLOT_PEPPY_CONFIG,
            Some(
                r#"{
                    main: [{ core_node: "core-1234", instance_id: "camera_1" }],
                    wrist_camera: [],
                    arms: [{ core_node: "core-1234", instance_id: "right_arm" }]
                }"#,
            ),
        )
        .expect("missing zero_or_more entry should construct");
        assert!(processor.bound_producers("spare_cameras").is_empty());
    }

    /// Manifest fixture with one observer slot of each cardinality. Every
    /// declared observer slot is consumed, which the manifest parser requires.
    const OBSERVER_PEPPY_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "consumer_node",
            tag: "v1",
            depends_on: {
                pairing_observers: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "sole_arm" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "watched_arms", cardinality: "one_or_more" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "spare_arms", cardinality: "zero_or_more" }
                ]
            }
        },
        interfaces: {
            topics: {
                consumes: [
                    { link_id: "sole_arm", name: "joint_states" },
                    { link_id: "watched_arms", name: "joint_states" },
                    { link_id: "spare_arms", name: "joint_states" }
                ]
            }
        },
        execution: { language: "rust", run_cmd: ["./target/debug/consumer_node"] },
    }"#;

    /// Like [`OBSERVER_PEPPY_CONFIG`] but with only the cardinalities whose
    /// empty state is a legal plan (`zero_or_one` vacant, `zero_or_more`
    /// observing nothing), so a seedless boot config constructs.
    const ZERO_FLOOR_OBSERVER_PEPPY_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "consumer_node",
            tag: "v1",
            depends_on: {
                pairing_observers: [
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "maybe_arm", cardinality: "zero_or_one" },
                    { name: "arm_link", tag: "v1", role: "arm", link_id: "spare_arms", cardinality: "zero_or_more" }
                ]
            }
        },
        interfaces: {
            topics: {
                consumes: [
                    { link_id: "maybe_arm", name: "joint_states" },
                    { link_id: "spare_arms", name: "joint_states" }
                ]
            }
        },
        execution: { language: "rust", run_cmd: ["./target/debug/consumer_node"] },
    }"#;

    /// A boot config without `observation_seeds` boots a zero-floor slot
    /// empty: that is a legal plan (vacant / observing nothing), never a
    /// missing delivery.
    #[test]
    fn zero_floor_observer_slots_boot_empty_without_seeds() {
        let processor = daemon_processor_with_bindings(ZERO_FLOOR_OBSERVER_PEPPY_CONFIG, None)
            .expect("zero-floor observer slots need no seed");

        for (link_id, expected) in [
            ("maybe_arm", Cardinality::ZeroOrOne),
            ("spare_arms", Cardinality::ZeroOrMore),
        ] {
            assert_eq!(
                processor.observation_slot_cardinality(link_id),
                Some(expected),
                "slot `{link_id}` must carry its declared cardinality"
            );
            let watch = processor
                .observation_slot_watch(link_id)
                .unwrap_or_else(|| panic!("slot `{link_id}` must have a channel"));
            assert!(watch.borrow().members.is_empty());
        }

        assert_eq!(processor.observation_slot_cardinality("not_declared"), None);
        assert!(processor.observation_slot_watch("not_declared").is_none());
    }

    /// The observation twin of `fails_at_startup_when_declared_slot_has_no_binding`:
    /// a missing seed counts as empty, and an empty `one` / `one_or_more`
    /// slot is a set the plan cannot have written, so construction fails as
    /// version skew (a daemon that predates seeding) instead of booting a
    /// slot in an impossible state.
    #[test]
    fn a_missing_seed_for_a_floored_observer_slot_fails_startup() {
        let Err(err) = daemon_processor_with_bindings(OBSERVER_PEPPY_CONFIG, None) else {
            panic!("expected missing-seed startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::ObservationSeedViolated { link_id, cardinality, seeded }
                    if link_id == "sole_arm" && *cardinality == Cardinality::One && *seeded == 0
            ),
            "expected ObservationSeedViolated for `sole_arm`, got: {err}"
        );
    }

    /// A seed whose size the declared cardinality does not allow fails the
    /// same way: the launcher validated the plan, so an oversized set is an
    /// incompatible component, not a user error.
    #[test]
    fn an_oversized_seed_fails_startup() {
        let Err(err) = daemon_processor_with_instance_block(
            ZERO_FLOOR_OBSERVER_PEPPY_CONFIG,
            r#"{
                instance_id: "consumer_1",
                observation_seeds: {
                    maybe_arm: [
                        {
                            source: { core_node: "core-1234", instance_id: "arm_1" },
                            source_link_id: "controller",
                            source_generation: 1,
                            source_live: true
                        },
                        {
                            source: { core_node: "core-1234", instance_id: "arm_2" },
                            source_link_id: "controller",
                            source_generation: 1,
                            source_live: true
                        }
                    ]
                }
            }"#,
        ) else {
            panic!("expected oversized-seed startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::ObservationSeedViolated { link_id, cardinality, seeded }
                    if link_id == "maybe_arm"
                        && *cardinality == Cardinality::ZeroOrOne
                        && *seeded == 2
            ),
            "expected ObservationSeedViolated for `maybe_arm`, got: {err}"
        );
    }

    /// The seed path holds the wire decoder's line: one observed pairing may
    /// appear only once in a slot's member set, so a hand-edited boot config
    /// cannot open duplicate subscriptions.
    #[test]
    fn a_duplicate_seed_identity_fails_startup() {
        let Err(err) = daemon_processor_with_instance_block(
            ZERO_FLOOR_OBSERVER_PEPPY_CONFIG,
            r#"{
                instance_id: "consumer_1",
                observation_seeds: {
                    spare_arms: [
                        {
                            source: { core_node: "core-1234", instance_id: "arm_1" },
                            source_link_id: "controller",
                            source_generation: 1,
                            source_live: true
                        },
                        {
                            source: { core_node: "core-1234", instance_id: "arm_1" },
                            source_link_id: "controller",
                            source_generation: 1,
                            source_live: true
                        }
                    ]
                }
            }"#,
        ) else {
            panic!("expected duplicate-seed startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::ObservationSeedDuplicate { link_id, instance_id, .. }
                    if link_id == "spare_arms" && instance_id == "arm_1"
            ),
            "expected ObservationSeedDuplicate, got: {err}"
        );
    }

    /// A seed for a slot the manifest does not declare is version skew and
    /// fails construction rather than being silently dropped.
    #[test]
    fn a_seed_for_an_undeclared_slot_fails_startup() {
        let Err(err) = daemon_processor_with_instance_block(
            ZERO_FLOOR_OBSERVER_PEPPY_CONFIG,
            r#"{
                instance_id: "consumer_1",
                observation_seeds: {
                    phantom_slot: []
                }
            }"#,
        ) else {
            panic!("expected undeclared-seed startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::ObservationSeedUndeclared { link_id } if link_id == "phantom_slot"
            ),
            "expected ObservationSeedUndeclared, got: {err}"
        );
    }

    /// A boot config's `observation_seeds` is each slot's first delivery: the
    /// member set answers from construction (so setup-time discovery sees the
    /// plan's membership), in plan order, stamped exactly as the daemon wrote
    /// it. An unseeded slot in the same config still boots empty, and a live
    /// `observation_update` (whose sequence is always positive) replaces the
    /// sequence-zero seed.
    #[test]
    fn observation_seeds_answer_from_construction() {
        let processor = daemon_processor_with_instance_block(
            OBSERVER_PEPPY_CONFIG,
            r#"{
                instance_id: "consumer_1",
                observation_seeds: {
                    sole_arm: [
                        {
                            source: { core_node: "core-1234", instance_id: "arm_1" },
                            source_link_id: "controller",
                            source_generation: 3,
                            source_live: true
                        }
                    ],
                    watched_arms: [
                        {
                            source: { core_node: "core-1234", instance_id: "arm_9" },
                            source_link_id: "controller",
                            source_generation: 2,
                            source_live: true
                        },
                        {
                            source: { core_node: "remote-5678", instance_id: "arm_3" },
                            source_link_id: "controller",
                            source_generation: 0,
                            source_live: false
                        }
                    ]
                }
            }"#,
        )
        .expect("a seeded observer config should construct");

        let sole = processor
            .observation_slot_watch("sole_arm")
            .expect("declared slot has a channel");
        let state = sole.borrow().clone();
        assert_eq!(state.sequence, 0, "the seed is the pre-delivery state");
        assert_eq!(state.members.len(), 1);
        assert_eq!(state.members[0].source.producer.instance_id, "arm_1");
        assert_eq!(state.members[0].source.source_link_id, "controller");
        assert_eq!(state.members[0].source_generation, 3);
        assert!(state.members[0].source_live);

        let watched = processor
            .observation_slot_watch("watched_arms")
            .expect("declared slot has a channel");
        let members = watched.borrow().members.clone();
        assert_eq!(
            members
                .iter()
                .map(|m| m.source.producer.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["arm_9", "arm_3"],
            "plan order is preserved through the seed (arm_9 sorts after arm_3, \
             so a sorted set would flip this)"
        );
        assert!(
            !members[1].source_live,
            "a member whose source has not run yet is seeded down, membership intact"
        );

        let spare = processor
            .observation_slot_watch("spare_arms")
            .expect("declared slot has a channel");
        assert!(
            spare.borrow().members.is_empty(),
            "an unseeded slot boots empty exactly as before"
        );
    }

    /// Standalone runs enforce the same rules via
    /// `StandaloneConfig::with_bound_producer`: a declared `one` slot left
    /// unseeded fails startup; repeat calls accumulate a multi slot's set
    /// in call order; duplicate seeds are rejected.
    #[test]
    fn standalone_bound_producers_accumulate_and_enforce_cardinality() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, MULTI_SLOT_PEPPY_CONFIG)
            .expect("peppy config should be written");

        let unseeded = StandaloneConfig::new();
        let Err(err) = Processor::new_standalone(&peppy_config_path, &unseeded) else {
            panic!("expected unbound-slot startup error");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::SlotUnbound { link_id, .. } if link_id == "main"
            ),
            "expected SlotUnbound for `main`, got: {err}"
        );

        // `spare_cameras` (zero_or_more) is deliberately left unseeded.
        let seeded = StandaloneConfig::new()
            .with_bound_producer("main", "core_x", "camera_1")
            .with_bound_producer("wrist_camera", "core_x", "camera_2")
            .with_bound_producer("arms", "core_x", "right_arm")
            .with_bound_producer("arms", "core_x", "left_arm");
        let processor = Processor::new_standalone(&peppy_config_path, &seeded)
            .expect("seeded slots should construct");
        assert_eq!(
            instance_ids(processor.bound_producers("main")),
            ["camera_1"]
        );
        assert_eq!(
            processor
                .optional_bound_producer("wrist_camera")
                .map(|producer| producer.instance_id.as_str()),
            Some("camera_2")
        );
        assert_eq!(
            instance_ids(processor.bound_producers("arms")),
            ["right_arm", "left_arm"],
            "with_bound_producer call order must be preserved"
        );
        assert!(processor.bound_producers("spare_cameras").is_empty());

        let duplicated = StandaloneConfig::new()
            .with_bound_producer("main", "core_x", "camera_1")
            .with_bound_producer("arms", "core_x", "right_arm")
            .with_bound_producer("arms", "core_x", "right_arm");
        let Err(err) = Processor::new_standalone(&peppy_config_path, &duplicated) else {
            panic!("duplicate seeded producer must be rejected");
        };
        assert!(
            err.to_string().contains("right_arm"),
            "duplicate error should name the producer, got: {err}"
        );
    }

    /// `with_vacant_producer_slot` is the standalone spelling of a
    /// deployment's `{ vacant: "<why>" }`: it seeds the explicit empty set a
    /// launcher would write, the slot's accessor answers `None`, and the same
    /// call on a slot whose cardinality has a floor of one fails startup.
    #[test]
    fn standalone_vacant_producer_slot_seeds_an_explicit_empty_set() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, MULTI_SLOT_PEPPY_CONFIG)
            .expect("peppy config should be written");

        let vacant = StandaloneConfig::new()
            .with_bound_producer("main", "core_x", "camera_1")
            .with_vacant_producer_slot("wrist_camera")
            .with_bound_producer("arms", "core_x", "right_arm");
        let processor = Processor::new_standalone(&peppy_config_path, &vacant)
            .expect("a vacant zero_or_one slot should construct");
        assert_eq!(processor.optional_bound_producer("wrist_camera"), None);

        let vacant_one_slot = StandaloneConfig::new()
            .with_vacant_producer_slot("main")
            .with_vacant_producer_slot("wrist_camera")
            .with_bound_producer("arms", "core_x", "right_arm");
        let Err(err) = Processor::new_standalone(&peppy_config_path, &vacant_one_slot) else {
            panic!("a vacant `one` slot must fail startup");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::SlotCardinalityViolated { link_id, cardinality, bound }
                    if link_id == "main" && *cardinality == Cardinality::One && *bound == 0
            ),
            "expected SlotCardinalityViolated for `main`, got: {err}"
        );
    }

    /// `StandaloneConfig::with_observed_source` is the observer counterpart of
    /// `with_bound_producer`: a declared `one` slot left unseeded fails startup,
    /// repeat calls accumulate a multi slot's set in call order, and the seeded
    /// members answer the slot's accessors from construction.
    #[test]
    fn standalone_observed_sources_accumulate_and_enforce_cardinality() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, OBSERVER_PEPPY_CONFIG)
            .expect("peppy config should be written");

        let unseeded = StandaloneConfig::new();
        let Err(err) = Processor::new_standalone(&peppy_config_path, &unseeded) else {
            panic!("expected an unseeded floored observer slot to fail startup");
        };
        assert!(
            matches!(
                &err,
                crate::error::Error::ObservationSeedViolated { link_id, cardinality, seeded }
                    if link_id == "sole_arm" && *cardinality == Cardinality::One && *seeded == 0
            ),
            "expected ObservationSeedViolated for `sole_arm`, got: {err}"
        );

        // `spare_arms` (zero_or_more) is deliberately left unseeded.
        let seeded = StandaloneConfig::new()
            .with_observed_source("sole_arm", "core_x", "left_arm", "commander")
            .with_observed_source("watched_arms", "core_x", "right_arm", "commander")
            .with_observed_source("watched_arms", "core_x", "left_arm", "commander");
        let processor = Processor::new_standalone(&peppy_config_path, &seeded)
            .expect("seeded observer slots should construct");

        let sole = processor
            .observation_slot_watch("sole_arm")
            .expect("declared slot has a watch channel");
        assert_eq!(
            sole.borrow()
                .members
                .iter()
                .map(|m| m.source.producer.instance_id.clone())
                .collect::<Vec<_>>(),
            ["left_arm"]
        );

        let watched = processor
            .observation_slot_watch("watched_arms")
            .expect("declared slot has a watch channel");
        assert_eq!(
            watched
                .borrow()
                .members
                .iter()
                .map(|m| m.source.producer.instance_id.clone())
                .collect::<Vec<_>>(),
            ["right_arm", "left_arm"],
            "with_observed_source call order must be preserved"
        );

        let spare = processor
            .observation_slot_watch("spare_arms")
            .expect("declared slot has a watch channel");
        assert!(spare.borrow().members.is_empty());

        let duplicated = StandaloneConfig::new()
            .with_observed_source("sole_arm", "core_x", "left_arm", "commander")
            .with_observed_source("watched_arms", "core_x", "right_arm", "commander")
            .with_observed_source("watched_arms", "core_x", "right_arm", "commander");
        let Err(err) = Processor::new_standalone(&peppy_config_path, &duplicated) else {
            panic!("duplicate seeded pairing must be rejected");
        };
        assert!(
            err.to_string().contains("right_arm"),
            "duplicate error should name the source, got: {err}"
        );
    }

    /// An undeclared link_id is a standalone typo, not a manifest violation, so
    /// it warns and is dropped rather than failing startup, on the same terms as
    /// `with_bound_producer`.
    #[test]
    fn standalone_observed_source_on_an_undeclared_slot_is_ignored() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let peppy_config_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&peppy_config_path, ZERO_FLOOR_OBSERVER_PEPPY_CONFIG)
            .expect("peppy config should be written");

        let stray = StandaloneConfig::new().with_observed_source(
            "not_a_slot",
            "core_x",
            "left_arm",
            "commander",
        );
        let processor = Processor::new_standalone(&peppy_config_path, &stray)
            .expect("an undeclared observer seed should be ignored, not fatal");
        assert!(processor.observation_slot_watch("not_a_slot").is_none());
    }
}
