mod clock;
mod core_node_name;
pub use clock::{ClockBinding, ClockDomainId, ClockIncarnation, ClockRole, InvalidIncarnation};
pub use core_node_name::{CoreNodeName, CoreNodeNameError, MAX_CORE_NODE_NAME_LEN, SELF_CORE_NODE};

use crate::common::AnyType;
use crate::consts::ALLOWED_CONFIG_CHARS;
use crate::error::{ParsingError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Validated identifier for node names, tags, instance ids, and core node
/// names in runtime configs: non-empty and restricted to
/// [`ALLOWED_CONFIG_CHARS`](crate::consts::ALLOWED_CONFIG_CHARS).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
pub struct Name(String);

impl Name {
    pub fn new<S: Into<String>>(s: S) -> std::result::Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        ALLOWED_CONFIG_CHARS.contains(c)
    }
}

impl TryFrom<String> for Name {
    type Error = ParsingError;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::EmptyName);
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        Err(ParsingError::InvalidName(
            value,
            ALLOWED_CONFIG_CHARS.to_string(),
        ))
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Name::try_from(s).map_err(|err| serde::de::Error::custom(err.to_string()))
    }
}

impl From<Name> for String {
    fn from(v: Name) -> Self {
        v.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        *self == other.0
    }
}

impl PartialOrd for Name {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// Fully-qualified producer address. The wire addresses a producer by the
/// `(core_node, instance_id)` pair — `instance_id` alone is only unique
/// within one stack, while the pair is unique across the whole mesh — so
/// every reference to a producer below the validator carries both halves.
/// The validator stamps `core_node` when it materializes bindings (the
/// `validate_bindings` pass in the peppy `daemon-config` crate); after
/// that point a half-address is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProducerRef {
    pub core_node: String,
    pub instance_id: String,
}

impl ProducerRef {
    pub fn new(core_node: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
        }
    }
}

/// The first item repeating one before it, in declaration order.
///
/// The shared half of every distinct-members gate: each caller decides what
/// an empty list means and names its own refusal for the item returned here.
pub fn first_duplicate<T: Eq + std::hash::Hash>(items: &[T]) -> Option<&T> {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().find(|item| !seen.insert(*item))
}

/// The runtime-resolved, immutable, ordered producer set bound to one
/// consumer slot. Order is the application declaration order (launcher
/// array order / CLI flag occurrence order), preserved verbatim from the
/// validator through boot configs to the generated bound-producer
/// accessors, so selecting the first member is deterministic. Duplicates
/// are rejected rather than removed or
/// reordered. The set's validated size is the slot's declared
/// `cardinality`: exactly one for `one` (the default), at most one for
/// `zero_or_one`, one or more for `one_or_more`, zero or more for
/// `zero_or_more`. An empty set has no bound edge, and it is the resolved
/// form of two things: a `zero_or_more` slot the application bound nothing
/// to, and a `zero_or_one` slot the deployment wrote vacant. The set is
/// fixed when the node starts; producers disconnecting at runtime never
/// shrink it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundProducers(Vec<ProducerRef>);

impl BoundProducers {
    pub fn as_slice(&self) -> &[ProducerRef] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ProducerRef> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn first(&self) -> Option<&ProducerRef> {
        self.0.first()
    }
}

/// A one-producer set, for `cardinality: "one"` slots and tests.
impl From<ProducerRef> for BoundProducers {
    fn from(producer: ProducerRef) -> Self {
        Self(vec![producer])
    }
}

/// Ordered construction from an already-collected target list, rejecting
/// duplicates. The single construction gate: the deserializer delegates
/// here, and the launcher validator calls it when it materializes a
/// slot's set, so every boundary rejects the same sets with the same
/// error.
impl TryFrom<Vec<ProducerRef>> for BoundProducers {
    type Error = ParsingError;

    fn try_from(producers: Vec<ProducerRef>) -> std::result::Result<Self, Self::Error> {
        // The first duplicated producer in declaration order names the error.
        if let Some(duplicate) = first_duplicate(&producers) {
            return Err(ParsingError::DuplicateBoundProducer {
                core_node: duplicate.core_node.clone(),
                instance_id: duplicate.instance_id.clone(),
            });
        }
        Ok(Self(producers))
    }
}

impl<'a> IntoIterator for &'a BoundProducers {
    type Item = &'a ProducerRef;
    type IntoIter = std::slice::Iter<'a, ProducerRef>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Custom deserializer so the two failure shapes give actionable errors
/// instead of generic serde type mismatches: a duplicate producer names the
/// duplicated instance, and an object payload (the removed pre-cardinality
/// single-producer shape) is called out as component version skew, since
/// the daemon, CLI, generated bindings, and node runtime must be released
/// together across the cardinality break.
impl<'de> Deserialize<'de> for BoundProducers {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        struct BoundProducersVisitor;

        impl<'de> serde::de::Visitor<'de> for BoundProducersVisitor {
            type Value = BoundProducers;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an ordered array of {core_node, instance_id} producers")
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut producers: Vec<ProducerRef> =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(producer) = seq.next_element::<ProducerRef>()? {
                    producers.push(producer);
                }
                BoundProducers::try_from(producers).map_err(serde::de::Error::custom)
            }

            fn visit_map<A>(self, _map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                Err(serde::de::Error::custom(
                    "slot binding uses the removed single-producer object shape; since the \
                     cardinality release a slot binds an ordered ARRAY of producers (a \
                     `cardinality: \"one\"` slot binds a one-element array). The daemon, CLI, \
                     generated bindings, and node runtime must be upgraded together",
                ))
            }
        }

        deserializer.deserialize_any(BoundProducersVisitor)
    }
}

/// The slot-binding map that travels boot configs, `node_info` responses,
/// and the daemon graph: consumer slot `link_id` → the ordered producer
/// set explicitly bound to that slot. The launcher validator materializes
/// one entry per declared `depends_on.{nodes,contracts}` slot at plan
/// time, sized per the slot's `cardinality`; an empty set is valid on a
/// `zero_or_more` slot and on a `zero_or_one` slot the deployment wrote
/// vacant. Every member is a full wire address; there is no wildcard, no
/// unbound state, and no discovery fallback.
pub type SlotBindings = BTreeMap<String, BoundProducers>;

/// One pair a participant pairing slot holds: the peer instance's wire
/// address, the link_id of the peer's own complementary slot, and the copy
/// the peer's instance belongs to (`None` for an instance run outside a
/// copy). A node holding several pairs from several copies groups them by
/// `copy`. The boot-config and node-info twin of the wire's `PeerMember`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PairedPeer {
    pub peer: ProducerRef,
    pub peer_link_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Name>,
}

/// The pairs of every participant pairing slot (a `depends_on.pairings`
/// entry) of a node instance, keyed by slot link_id, each set in
/// establishment order. A scalar slot holds zero or one; a multi slot holds
/// what its cardinality admits. Deliberately NOT part of `slot_bindings`:
/// slot bindings feed the immutable consumer-filter cache, while a pairing
/// slot is live-mutable over the node's lifetime (the daemon delivers its
/// set via the `peer_update` service). In boot configs every declared slot
/// is empty: all pairs, including those requested at `node run`, arrive over
/// the live channel after the instance commits to Running.
pub type PairingSlots = BTreeMap<String, Vec<PairedPeer>>;

/// The other end of the pair an observation is pinned to: the peer instance
/// the observed source publishes to, and the link_id of the peer's slot in
/// that pair. An observation the plan names by the peer's end carries one; an
/// observation named by the source alone observes every pair of the source's
/// slot and carries none.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct ObservedPeer {
    pub peer: ProducerRef,
    pub peer_link_id: String,
}

/// One member of one observer slot as the daemon stamped it at spawn time:
/// the observed pairing's identity plus that source's incarnation generation
/// and liveness at assembly. The boot-config twin of the wire's
/// `ObservedMemberState`: identical stamping, different transport, so the
/// node's first live `observation_update` normally repeats this state
/// byte-for-byte and replaces it without redeclaring any subscription.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationSeedMember {
    pub source: ProducerRef,
    pub source_link_id: String,
    /// The pair this member is pinned to, by its other end; `None` observes
    /// every pair of the source's slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<ObservedPeer>,
    pub source_generation: u64,
    pub source_live: bool,
}

/// Boot-time member sets of every observer slot (a `depends_on.pairing_observers`
/// entry) of a node instance, keyed by slot link_id, each set in plan order.
///
/// Deliberately UNLIKE `pairing_slots` (which boots empty): observer
/// membership is launch-time configuration the same way `slot_bindings` is,
/// and the daemon delivers it to a running instance only after that instance
/// reaches Running, which a node's setup runs strictly before. Without the
/// seed, setup-time discovery reads every slot empty with nothing to
/// distinguish "not delivered yet" from "bound to nothing". The seed is the
/// slot's first delivery, at sequence zero; live `observation_update`
/// deliveries carry strictly larger sequences and replace it wholesale.
pub type ObservationSeeds = BTreeMap<String, Vec<ObservationSeedMember>>;

/// Represents a node instance at runtime. Used by RuntimeConfig to identify the running node and its configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInstanceConfig {
    pub instance_id: Name,
    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run outside a copy. Read by nodes that hold pairs
    /// from several copies and tell the copies apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Name>,
    #[serde(default)]
    pub arguments: BTreeMap<String, AnyType>,
    #[serde(default)]
    pub framework: ResolvedFramework,
    /// The pre-resolved producer for every `link_id` declared in the
    /// consumer manifest's `depends_on.{nodes,contracts}`. Built by the
    /// validator from the launcher / CLI binding map — each target stamped
    /// with the launching daemon's `core_node` — so the spawned node does
    /// no re-resolution work and always holds a wire-complete producer
    /// address. Empty when the manifest has no `depends_on` entries.
    /// Read by the generated subscribe / poll / send_goal call sites via
    /// the runtime's per-slot bound-producer cache.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: SlotBindings,
    /// Boot-time pairs of every pairing slot declared in
    /// `depends_on.pairings`, keyed by slot link_id. Every declared slot boots
    /// empty: pairs requested via `--link` / launcher `links:` are delivered
    /// live over the `peer_update` service after the instance commits to
    /// Running, so there is exactly one delivery mechanism. Empty when the
    /// manifest declares no pairings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pairing_slots: PairingSlots,
    /// Boot-time member sets of the instance's observer slots, stamped by
    /// the daemon at spawn (see [`ObservationSeeds`]). Empty when the
    /// manifest declares no `pairing_observers`, and on a slot the plan
    /// left with nothing to observe.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub observation_seeds: ObservationSeeds,
}

impl NodeInstanceConfig {
    /// Builds a config with everything except `instance_id` defaulted:
    /// empty arguments, default framework, empty slot bindings. Use with
    /// struct-update syntax to override a field:
    /// `NodeInstanceConfig { arguments, ..NodeInstanceConfig::new(id) }`.
    pub fn new(instance_id: Name) -> Self {
        Self {
            instance_id,
            copy: None,
            arguments: BTreeMap::new(),
            framework: ResolvedFramework::default(),
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            observation_seeds: BTreeMap::new(),
        }
    }
}

/// Everything a daemon needs to start one node instance, and nothing that
/// would let the requester dictate the node's runtime identity.
///
/// This is what travels on a `node_run` goal. It deliberately carries no
/// messaging endpoint, no `bound_core_node`, and no resolved framework
/// values: a daemon owns the runtime identity of every node it spawns, so
/// those are supplied by the daemon that does the spawning, on every path.
/// A federated launch makes the reason concrete (a coordinator-assembled
/// endpoint names the coordinator's own router, which no node on a peer can
/// reach), but the rule is not federation-specific and the type is what
/// keeps it checkable in one place.
///
/// Turn one into the config a node actually receives with
/// [`NodeInstancePlan::resolve`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInstancePlan {
    pub instance_id: Name,
    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run outside a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Name>,
    #[serde(default)]
    pub arguments: BTreeMap<String, AnyType>,
    /// The one clock this instance reads for its lifetime, and its role in
    /// that clock's domain. Resolved by whoever planned the change, a launch's
    /// coordinator or the CLI, because a domain's identity spans machines and
    /// the spawning daemon knows only its own. Wall time is the default.
    #[serde(default, skip_serializing_if = "ClockBinding::is_wall")]
    pub clock: ClockBinding,
    /// Resolved producers per consumer slot, each already stamped with the
    /// core node it lives on, so a producer on another daemon addresses
    /// identically to a local one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: SlotBindings,
}

impl NodeInstancePlan {
    /// Builds a plan with everything except `instance_id` defaulted. Use
    /// with struct-update syntax to override a field.
    pub fn new(instance_id: Name) -> Self {
        Self {
            instance_id,
            copy: None,
            arguments: BTreeMap::new(),
            clock: ClockBinding::Wall,
            slot_bindings: BTreeMap::new(),
        }
    }

    /// Turns this plan into the config a node receives. The single place a
    /// plan becomes a config, which every spawn path goes through.
    ///
    /// The clock travels whole: a domain's identity names the machine its
    /// publisher runs on, so it means the same thing on every daemon that
    /// reads it. A daemon still refuses a publisher plan for a domain hosted
    /// elsewhere, which it checks where it knows its own name.
    pub fn resolve(self) -> NodeInstanceConfig {
        NodeInstanceConfig {
            instance_id: self.instance_id,
            copy: self.copy,
            arguments: self.arguments,
            framework: ResolvedFramework { clock: self.clock },
            slot_bindings: self.slot_bindings,
            pairing_slots: BTreeMap::new(),
            // Stamped by the spawning daemon after resolution: the seed
            // carries daemon-held state (source generations and liveness)
            // that a plan, by design, does not know.
            observation_seeds: BTreeMap::new(),
        }
    }
}

/// Framework knobs already resolved by the daemon. Distinct from
/// the launcher-file `FrameworkOverrides` (peppy `daemon-config`) so the type system enforces "resolution
/// happens once": the launcher form carries optional overrides; this form
/// carries concrete values the spawned node reads without further fallback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFramework {
    /// See [`NodeInstancePlan::clock`].
    #[serde(default, skip_serializing_if = "ClockBinding::is_wall")]
    pub clock: ClockBinding,
}

fn default_true() -> bool {
    true
}

fn default_standard_buffer_size() -> usize {
    crate::peppy_config::DEFAULT_STANDARD_BUFFER_SIZE
}

fn default_high_throughput_buffer_size() -> usize {
    crate::peppy_config::DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
}

fn default_daemon_grace_secs() -> u64 {
    crate::peppy_config::DEFAULT_DAEMON_GRACE_SECS
}

fn default_shutdown_grace_secs() -> u64 {
    crate::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS
}

/// Node lifecycle settings the daemon resolves once (from `peppy_config.json5`)
/// and ships to each spawned node. `daemon_grace_secs` is the grace period the
/// node's daemon-liveness watchdog waits, after the daemon's heartbeat goes
/// silent, before shutting itself down — the uncatchable-death safety net.
/// `shutdown_grace_secs` is the cooperative-shutdown window: the daemon waits
/// this long for a stopping node to exit before SIGKILL, and the node runtime
/// bounds its registered shutdown hooks by the same window so cleanup can never
/// hang a stop (or outlive a dead daemon) indefinitely.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRuntimeConfig {
    #[serde(default = "default_daemon_grace_secs")]
    pub daemon_grace_secs: u64,
    #[serde(default = "default_shutdown_grace_secs")]
    pub shutdown_grace_secs: u64,
}

impl Default for LifecycleRuntimeConfig {
    fn default() -> Self {
        Self {
            daemon_grace_secs: default_daemon_grace_secs(),
            shutdown_grace_secs: default_shutdown_grace_secs(),
        }
    }
}

impl LifecycleRuntimeConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Messaging-session settings the daemon resolves once and ships to a node.
///
/// Nodes open a `peer` session that connects to a seed (the router) and then
/// forms direct peer-to-peer links with peers discovered via gossip, so data
/// stops relaying through the router. Discovery is gossip-only; there is no
/// multicast (it would bridge otherwise-independent peer groups on a shared
/// host, and a known seed already covers discovery).
///
/// The subscriber buffer sizes live here too. They are a subscriber-channel
/// concern rather than a discovery one, but co-locating them keeps a single
/// struct (and a single serialized block) travelling the daemon-to-node path,
/// since this is already the value threaded into the node's session at startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Gossip seed endpoints (full Zenoh endpoints, e.g. `"tcp/127.0.0.1:7448"`).
    /// Empty means "derive the router endpoint from `messaging_host:messaging_port`".
    #[serde(default)]
    pub seed_peers: Vec<String>,
    /// Enable gossip so peers form direct links. Setting this to `false` forces
    /// all traffic through the router (a rollback switch without a rebuild).
    #[serde(default = "default_true")]
    pub gossip: bool,
    /// Subscriber channel buffer for the `Standard` QoS tier (in-flight messages).
    #[serde(default = "default_standard_buffer_size")]
    pub standard_buffer_size: usize,
    /// Subscriber channel buffer for the `HighThroughput` QoS tier (e.g. sensor data).
    #[serde(default = "default_high_throughput_buffer_size")]
    pub high_throughput_buffer_size: usize,
    /// Workspace namespace stamped by the daemon so each spawned node opens
    /// its session under the same namespace as the daemon (routing context
    /// across the platform federation). `None` means "logged out" and resolves
    /// to the constant `local` namespace at session open. Typed: an invalid
    /// value fails runtime-config parsing instead of leaking toward a live
    /// session. Omitted from serialized configs when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<crate::internal::namespace::Namespace>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            seed_peers: Vec::new(),
            gossip: true,
            standard_buffer_size: crate::peppy_config::DEFAULT_STANDARD_BUFFER_SIZE,
            high_throughput_buffer_size: crate::peppy_config::DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
            namespace: None,
        }
    }
}

impl DiscoveryConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// This class is generated by the peppy daemon and then passed to each respective peppy node instances spawned by it
/// through `PEPPY_RUNTIME_CONFIG` env var. It's then deserialized in the process runtime to understand
/// how to communicate with the rest of the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub messaging_host: String,
    pub messaging_port: u16,
    pub node_name: Name,
    pub node_tag: Name,
    pub bound_core_node: Name,
    pub node_instance: NodeInstanceConfig,
    /// Peer-discovery settings. Defaulted (and omitted from serialized configs)
    /// for the common case, so launch configs written before this field existed
    /// still parse.
    #[serde(default, skip_serializing_if = "DiscoveryConfig::is_default")]
    pub discovery: DiscoveryConfig,
    /// Node lifecycle settings (daemon-liveness grace period). Defaulted and
    /// omitted from serialized configs for the common case, so launch configs
    /// written before this field existed still parse byte-identically.
    #[serde(default, skip_serializing_if = "LifecycleRuntimeConfig::is_default")]
    pub lifecycle: LifecycleRuntimeConfig,
}

impl RuntimeConfig {
    pub fn new(
        messaging_host: &str,
        messaging_port: u16,
        node_instance: NodeInstanceConfig,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        bound_core_node: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            messaging_host: messaging_host.to_owned(),
            messaging_port,
            node_instance,
            node_name: Name::new(node_name.into())?,
            node_tag: Name::new(node_tag.into())?,
            bound_core_node: Name::new(bound_core_node.into())?,
            discovery: DiscoveryConfig::default(),
            lifecycle: LifecycleRuntimeConfig::default(),
        })
    }

    /// This function is typically invoked by the `peppy` program
    /// to persist its launch configuration for `peppylib` or `peppygen` to pick it up.
    pub fn save_json5_launch_config(&self, to_path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = to_path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let serialized = serde_json5::to_string(self)
            .map_err(|err| crate::error::Error::Serialize(err.to_string()))?;
        fs::write(path, serialized)?;
        Ok(path.to_path_buf())
    }

    pub fn generate_peppy_config_fingerprint(peppy_config: impl AsRef<Path>) -> Result<String> {
        use sha2::{Digest, Sha256};
        let config_path = peppy_config.as_ref();
        let content = std::fs::read(config_path)?;
        let hash = Sha256::digest(&content);
        Ok(hash
            .iter()
            .fold(String::with_capacity(hash.len() * 2), |mut acc, b| {
                use std::fmt::Write;
                let _ = write!(acc, "{:02x}", b);
                acc
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use tempfile::TempDir;

    #[test]
    fn name_validation() {
        assert!(Name::new("robot").is_ok());
        assert!(Name::new("camera_v1").is_ok());

        assert!(Name::new("").is_err()); // empty not permitted
        assert!(Name::new("/").is_err()); // slash not permitted
        assert!(Name::new("/robot").is_err()); // slash not permitted
        assert!(Name::new("Robot").is_ok()); // capital now allowed
        assert!(Name::new("robot$cam").is_err()); // special
    }

    #[test]
    fn name_error_message() {
        let err = Name::new("Invalid!").unwrap_err();
        if let ParsingError::InvalidName(_, msg) = err {
            assert_eq!(msg, crate::consts::ALLOWED_CONFIG_CHARS);
        } else {
            panic!("Expected InvalidName error");
        }
    }

    fn runtime_config_from_json(instance_id: &str) -> Result<RuntimeConfig> {
        let json = r#"{
            messaging_host: "$MESSAGING_HOST",
            messaging_port: $MESSAGING_PORT,
            node_instance: {
                instance_id: "$INSTANCE_ID"
            },
            node_name: "camera",
            node_tag: "v1",
            bound_core_node: "core_node"
        }"#;

        let populated = json
            .replace("$INSTANCE_ID", instance_id)
            .replace("$MESSAGING_HOST", "127.0.0.1")
            .replace("$MESSAGING_PORT", "7448");
        serde_json5::from_str(&populated).map_err(|err| Error::Parsing(err.into()))
    }

    /// A clock binding round-trips through serialize/deserialize, and a
    /// runtime config carrying no `framework` key reads as wall time.
    #[test]
    fn resolved_framework_round_trip_and_wall_default() {
        let simulated: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: {
                    instance_id: "camera_front",
                    framework: { clock: { sim: {
                        domain: { name: "robot", core_node: "cn-sim", incarnation: 7 },
                        role: { consumer: {
                            publisher: { core_node: "cn-sim", instance_id: "sim_inst" }
                        } },
                    } } }
                },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node"
            }"#,
        )
        .unwrap();
        let clock = simulated.node_instance.framework.clock.clone();
        assert_eq!(clock.label(), "robot@cn-sim");
        assert_eq!(
            clock.publisher_ref(),
            Some(&ProducerRef::new("cn-sim", "sim_inst"))
        );

        let serialized = serde_json5::to_string(&simulated).unwrap();
        let reparsed: RuntimeConfig = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.node_instance.framework.clock, clock);

        let without_framework = runtime_config_from_json("camera_front").unwrap();
        assert!(without_framework.node_instance.framework.clock.is_wall());
    }

    fn domain(incarnation: u64) -> ClockDomainId {
        ClockDomainId::new(
            Name::new("robot").expect("valid name"),
            CoreNodeName::new("cn-sim").expect("valid core node name"),
            ClockIncarnation::try_from(incarnation).expect("non-zero"),
        )
    }

    fn plan(clock: ClockBinding) -> NodeInstancePlan {
        NodeInstancePlan {
            clock,
            ..NodeInstancePlan::new(Name::new("inst_1").expect("valid name"))
        }
    }

    /// The clock a change planned is the clock the node receives, in every
    /// role. A domain names the machine its publisher runs on, so the daemon
    /// that spawns the node supplies no part of it.
    #[test]
    fn resolve_carries_the_clock_whole() {
        let publisher = ClockBinding::publisher(domain(7));
        assert_eq!(plan(publisher.clone()).resolve().framework.clock, publisher);

        let consumer = ClockBinding::consumer(domain(7), ProducerRef::new("cn-sim", "sim_inst"));
        assert_eq!(plan(consumer.clone()).resolve().framework.clock, consumer);

        assert!(plan(ClockBinding::Wall).resolve().framework.clock.is_wall());
    }

    /// Wall time is the absence of a binding on the wire, so a plan and a
    /// config for an instance reading its own machine's clock carry no clock
    /// field at all.
    #[test]
    fn wall_is_omitted_from_serialized_plans_and_configs() {
        let wall = plan(ClockBinding::Wall);
        let serialized_plan = serde_json5::to_string(&wall).unwrap();
        assert!(
            !serialized_plan.contains("clock"),
            "wall carries no field: {serialized_plan}"
        );

        let serialized_config = serde_json5::to_string(&wall.resolve()).unwrap();
        assert!(
            !serialized_config.contains("clock"),
            "wall carries no field: {serialized_config}"
        );

        let simulated = serde_json5::to_string(&plan(ClockBinding::publisher(domain(7)))).unwrap();
        assert!(
            simulated.contains("robot") && simulated.contains("cn-sim"),
            "a domain travels whole: {simulated}"
        );
    }

    /// A launch config written before `lifecycle` existed parses with the
    /// default grace period, an explicit block round-trips, and a default
    /// lifecycle is omitted from the serialized form so existing configs stay
    /// byte-identical.
    #[test]
    fn lifecycle_config_default_and_round_trip() {
        let legacy = runtime_config_from_json("camera_front").unwrap();
        assert_eq!(legacy.lifecycle, LifecycleRuntimeConfig::default());
        assert_eq!(
            legacy.lifecycle.daemon_grace_secs,
            crate::peppy_config::DEFAULT_DAEMON_GRACE_SECS
        );

        // Default lifecycle is skipped on serialize.
        let serialized = serde_json5::to_string(&legacy).unwrap();
        assert!(
            !serialized.contains("lifecycle"),
            "default lifecycle should not be serialized: {serialized}"
        );

        // A partial lifecycle block fills the missing field from its default
        // and an explicit block round-trips.
        let custom: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: { instance_id: "camera_front" },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node",
                lifecycle: { daemon_grace_secs: 42 }
            }"#,
        )
        .unwrap();
        assert_eq!(custom.lifecycle.daemon_grace_secs, 42);
        assert_eq!(
            custom.lifecycle.shutdown_grace_secs,
            crate::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS
        );
        let reparsed: RuntimeConfig =
            serde_json5::from_str(&serde_json5::to_string(&custom).unwrap()).unwrap();
        assert_eq!(reparsed.lifecycle, custom.lifecycle);

        let custom_shutdown: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: { instance_id: "camera_front" },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node",
                lifecycle: { shutdown_grace_secs: 7 }
            }"#,
        )
        .unwrap();
        assert_eq!(custom_shutdown.lifecycle.shutdown_grace_secs, 7);
        let reparsed: RuntimeConfig =
            serde_json5::from_str(&serde_json5::to_string(&custom_shutdown).unwrap()).unwrap();
        assert_eq!(reparsed.lifecycle, custom_shutdown.lifecycle);
    }

    /// A launch config written before `discovery` existed (no `discovery` key)
    /// parses with the gossip-on default, an explicit discovery block
    /// round-trips, and a default discovery is omitted from the serialized form
    /// so existing configs stay byte-identical.
    #[test]
    fn discovery_config_default_and_round_trip() {
        let legacy = runtime_config_from_json("camera_front").unwrap();
        assert_eq!(legacy.discovery, DiscoveryConfig::default());
        assert!(legacy.discovery.gossip);
        assert!(legacy.discovery.seed_peers.is_empty());
        // A launch config written before the buffer fields existed still parses
        // and gets the built-in defaults.
        assert_eq!(legacy.discovery.standard_buffer_size, 128);
        assert_eq!(legacy.discovery.high_throughput_buffer_size, 1024);

        // Default discovery is skipped on serialize.
        let serialized = serde_json5::to_string(&legacy).unwrap();
        assert!(
            !serialized.contains("discovery"),
            "default discovery should not be serialized: {serialized}"
        );

        // A discovery block that omits the buffer keys still parses (defaults).
        let no_buffers: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: { instance_id: "camera_front" },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node",
                discovery: { seed_peers: ["tcp/10.0.0.2:7448"], gossip: false }
            }"#,
        )
        .unwrap();
        assert_eq!(
            no_buffers.discovery.seed_peers,
            vec!["tcp/10.0.0.2:7448".to_string()]
        );
        assert!(!no_buffers.discovery.gossip);
        assert_eq!(no_buffers.discovery.standard_buffer_size, 128);
        assert_eq!(no_buffers.discovery.high_throughput_buffer_size, 1024);

        // Explicit buffer sizes round-trip.
        let custom: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: { instance_id: "camera_front" },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node",
                discovery: {
                    seed_peers: ["tcp/10.0.0.2:7448"],
                    gossip: false,
                    standard_buffer_size: 64,
                    high_throughput_buffer_size: 4096
                }
            }"#,
        )
        .unwrap();
        assert_eq!(custom.discovery.standard_buffer_size, 64);
        assert_eq!(custom.discovery.high_throughput_buffer_size, 4096);

        let reparsed: RuntimeConfig =
            serde_json5::from_str(&serde_json5::to_string(&custom).unwrap()).unwrap();
        assert_eq!(reparsed.discovery, custom.discovery);

        // A default discovery has no namespace and omits it on serialize.
        assert!(DiscoveryConfig::default().namespace.is_none());
        assert!(
            !serialized.contains("namespace"),
            "absent namespace should not be serialized: {serialized}"
        );

        // An explicit namespace round-trips and is emitted on serialize.
        let with_namespace: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: { instance_id: "camera_front" },
                node_name: "camera",
                node_tag: "v1",
                bound_core_node: "core_node",
                discovery: { namespace: "550e8400-e29b-41d4-a716-446655440000" }
            }"#,
        )
        .unwrap();
        assert_eq!(
            with_namespace
                .discovery
                .namespace
                .as_ref()
                .map(|namespace| namespace.as_str()),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        let namespace_serialized = serde_json5::to_string(&with_namespace).unwrap();
        assert!(
            namespace_serialized.contains("namespace"),
            "an explicit namespace should be serialized: {namespace_serialized}"
        );
        let reparsed: RuntimeConfig = serde_json5::from_str(&namespace_serialized).unwrap();
        assert_eq!(reparsed.discovery, with_namespace.discovery);
    }

    #[test]
    fn runtime_config_rejects_an_invalid_namespace() {
        assert!(
            serde_json5::from_str::<RuntimeConfig>(
                r#"{
                    messaging_host: "127.0.0.1",
                    messaging_port: 7448,
                    node_instance: { instance_id: "camera_front" },
                    node_name: "camera",
                    node_tag: "v1",
                    bound_core_node: "core_node",
                    discovery: { namespace: "**" }
                }"#,
            )
            .is_err(),
            "an invalid namespace must fail runtime-config parsing"
        );
    }

    #[test]
    fn writes_launch_config_and_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("peppy_launcher.json5");

        let config = runtime_config_from_json("camera_front").expect("runtime config should parse");
        let returned = config
            .save_json5_launch_config(&path)
            .expect("runtime config should write");

        let written = fs::read_to_string(&path).expect("launch config should be written to disk");
        let parsed: RuntimeConfig =
            serde_json5::from_str(&written).expect("launch config should parse");

        assert_eq!(returned, path);
        assert_eq!(parsed.node_name, "camera");
        assert_eq!(parsed.node_instance.instance_id, "camera_front");
        assert_eq!(parsed.bound_core_node, "core_node");
        assert_eq!(
            parsed.node_instance.instance_id,
            config.node_instance.instance_id
        );
        assert!(parsed.node_instance.arguments.is_empty());
    }

    /// Pin the wire contract of `slot_bindings`: each slot maps its
    /// `link_id` to the ORDERED ARRAY of full `(core_node, instance_id)`
    /// producer pairs bound to it — a one-element array for a
    /// `cardinality: "one"` slot, an empty array for an unbound
    /// `zero_or_more` slot and for a `zero_or_one` slot the deployment wrote
    /// vacant. A shape change here is a `graph_json` /
    /// launch-config wire break, so assert the exact JSON and that it
    /// round-trips with member order preserved.
    #[test]
    fn slot_bindings_serde_contract() {
        use serde_json::json;

        let bindings: SlotBindings = [
            (
                "main".to_string(),
                BoundProducers::from(ProducerRef::new("core_a", "p1")),
            ),
            (
                "camera".to_string(),
                BoundProducers::try_from(vec![
                    ProducerRef::new("core_a", "front_camera"),
                    ProducerRef::new("core_a", "rear_camera"),
                ])
                .expect("distinct producers"),
            ),
            ("spare".to_string(), BoundProducers::default()),
        ]
        .into_iter()
        .collect();

        let expected = json!({
            "camera": [
                { "core_node": "core_a", "instance_id": "front_camera" },
                { "core_node": "core_a", "instance_id": "rear_camera" }
            ],
            "main": [ { "core_node": "core_a", "instance_id": "p1" } ],
            "spare": []
        });

        let encoded = serde_json::to_value(&bindings).expect("serialize slot_bindings");
        assert_eq!(encoded, expected, "slot_bindings JSON shape changed");
        let decoded: SlotBindings =
            serde_json::from_value(expected).expect("deserialize slot_bindings");
        assert_eq!(decoded, bindings, "slot_bindings did not round-trip");
        assert_eq!(
            decoded
                .get("camera")
                .expect("camera slot")
                .iter()
                .map(|p| p.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["front_camera", "rear_camera"],
            "member order must survive the round-trip"
        );
    }

    /// The removed pre-cardinality single-producer object shape must fail
    /// with a message that names the break as component version skew, not a
    /// generic serde type error: the daemon, CLI, generated bindings, and
    /// node runtime ship together across this wire change.
    #[test]
    fn slot_bindings_reject_pre_cardinality_object_shape_with_clear_error() {
        use serde_json::json;

        let legacy_shape = json!({
            "main": { "core_node": "core_a", "instance_id": "p1" }
        });
        let err = serde_json::from_value::<SlotBindings>(legacy_shape)
            .expect_err("object-shaped slot binding must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("single-producer object shape"),
            "error must name the removed shape: {msg}"
        );
        assert!(
            msg.contains("upgraded together"),
            "error must call out the version-skew fix: {msg}"
        );
    }

    /// Malformed members and duplicate producers are hard parse errors:
    /// half-addresses, unknown fields on a pair, non-object members, and a
    /// producer appearing twice within one slot's set.
    #[test]
    fn slot_bindings_reject_malformed_members_and_duplicates() {
        use serde_json::json;

        let rejected = [
            // Half an address.
            json!([{ "instance_id": "p1" }]),
            json!([{ "core_node": "core_a" }]),
            // Unknown extra field on a pair.
            json!([{ "core_node": "core_a", "instance_id": "p1", "extra": 1 }]),
            // A bare string is not a producer pair.
            json!(["p1"]),
            // Duplicate producer within one slot.
            json!([
                { "core_node": "core_a", "instance_id": "p1" },
                { "core_node": "core_a", "instance_id": "p1" }
            ]),
        ];
        for payload in rejected {
            let result: std::result::Result<BoundProducers, _> =
                serde_json::from_value(payload.clone());
            assert!(
                result.is_err(),
                "payload must fail to parse as a slot's bound set, but parsed: {payload}"
            );
            let map_payload = json!({ "slot": payload });
            let map_result: std::result::Result<SlotBindings, _> =
                serde_json::from_value(map_payload.clone());
            assert!(
                map_result.is_err(),
                "payload must fail to parse inside slot_bindings, but parsed: {map_payload}"
            );
        }

        // The duplicate error names the duplicated producer.
        let dup = json!([
            { "core_node": "core_a", "instance_id": "front_camera" },
            { "core_node": "core_a", "instance_id": "front_camera" }
        ]);
        let msg = serde_json::from_value::<BoundProducers>(dup)
            .expect_err("duplicate must be rejected")
            .to_string();
        assert!(
            msg.contains("front_camera@core_a"),
            "duplicate error must name the producer: {msg}"
        );

        // Same-instance producers on different core nodes are distinct, not
        // duplicates.
        let cross_core = json!([
            { "core_node": "core_a", "instance_id": "cam" },
            { "core_node": "core_b", "instance_id": "cam" }
        ]);
        let parsed: BoundProducers =
            serde_json::from_value(cross_core).expect("distinct core nodes must parse");
        assert_eq!(parsed.len(), 2);
    }

    /// `BoundProducers::try_from` mirrors the deserializer: declaration
    /// order is preserved and duplicates are rejected (not deduplicated).
    #[test]
    fn bound_producers_try_from_preserves_order_and_rejects_duplicates() {
        let ordered = BoundProducers::try_from(vec![
            ProducerRef::new("core_a", "rear_camera"),
            ProducerRef::new("core_a", "front_camera"),
        ])
        .expect("distinct producers");
        assert_eq!(
            ordered
                .iter()
                .map(|p| p.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["rear_camera", "front_camera"],
            "declaration order must be preserved, not sorted"
        );
        assert_eq!(
            ordered.first().map(|p| p.instance_id.as_str()),
            Some("rear_camera")
        );

        let err = BoundProducers::try_from(vec![
            ProducerRef::new("core_a", "cam"),
            ProducerRef::new("core_a", "cam"),
        ])
        .expect_err("duplicates must be rejected");
        let ParsingError::DuplicateBoundProducer {
            core_node,
            instance_id,
        } = err
        else {
            panic!("expected DuplicateBoundProducer, got {err:?}");
        };
        assert_eq!(core_node, "core_a");
        assert_eq!(instance_id, "cam");
    }

    /// Pin the wire contract of `PairedPeer` (contrast with the plain-array
    /// `slot_bindings` shape pinned above): full `(core_node, instance_id)`
    /// peer address, the peer's slot link_id, and the peer's copy only when
    /// it has one. This shape travels boot configs and `stack list` output.
    #[test]
    fn paired_peer_serde_contract() {
        use serde_json::json;

        let cases = [
            (
                PairedPeer {
                    peer: ProducerRef::new("core_a", "arm_1"),
                    peer_link_id: "controller".to_string(),
                    copy: None,
                },
                json!({
                    "peer": { "core_node": "core_a", "instance_id": "arm_1" },
                    "peer_link_id": "controller"
                }),
            ),
            (
                PairedPeer {
                    peer: ProducerRef::new("core_a", "bravo_backbone_inst"),
                    peer_link_id: "left_arm_link".to_string(),
                    copy: Some(Name::new("bravo").unwrap()),
                },
                json!({
                    "peer": { "core_node": "core_a", "instance_id": "bravo_backbone_inst" },
                    "peer_link_id": "left_arm_link",
                    "copy": "bravo"
                }),
            ),
        ];
        for (value, expected) in cases {
            let encoded = serde_json::to_value(&value).expect("serialize PairedPeer");
            assert_eq!(encoded, expected, "PairedPeer JSON shape changed");
            let decoded: PairedPeer =
                serde_json::from_value(expected).expect("deserialize PairedPeer");
            assert_eq!(decoded, value, "PairedPeer did not round-trip");
        }
    }

    /// A runtime config that names no `pairing_slots` parses with an empty
    /// map, and an empty map is omitted on serialize so such configs stay
    /// byte-identical. A slot's value is its pairs in order: none, one, or
    /// several.
    #[test]
    fn pairing_slots_default_and_round_trip() {
        let bare = runtime_config_from_json("camera_front").unwrap();
        assert!(bare.node_instance.pairing_slots.is_empty());
        let serialized = serde_json5::to_string(&bare).unwrap();
        assert!(
            !serialized.contains("pairing_slots"),
            "empty pairing_slots should not be serialized: {serialized}"
        );

        let with_slots: RuntimeConfig = serde_json5::from_str(
            r#"{
                messaging_host: "127.0.0.1",
                messaging_port: 7448,
                node_instance: {
                    instance_id: "engine_1",
                    copy: "sim",
                    pairing_slots: {
                        arm: [],
                        left_arm: [
                            { peer: { core_node: "core_a", instance_id: "alpha_backbone_inst" }, peer_link_id: "left_arm_link", copy: "alpha" },
                            { peer: { core_node: "core_b", instance_id: "bravo_backbone_inst" }, peer_link_id: "left_arm_link", copy: "bravo" }
                        ]
                    }
                },
                node_name: "sim_engine",
                node_tag: "v1",
                bound_core_node: "core_node"
            }"#,
        )
        .unwrap();
        assert_eq!(
            with_slots.node_instance.copy,
            Some(Name::new("sim").unwrap())
        );
        assert_eq!(
            with_slots.node_instance.pairing_slots.get("arm"),
            Some(&Vec::new())
        );
        let left_arm = &with_slots.node_instance.pairing_slots["left_arm"];
        assert_eq!(
            left_arm
                .iter()
                .map(|pair| pair.peer.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["alpha_backbone_inst", "bravo_backbone_inst"],
            "pairs keep their order"
        );
        assert_eq!(left_arm[1].copy, Some(Name::new("bravo").unwrap()));
        let reparsed: RuntimeConfig =
            serde_json5::from_str(&serde_json5::to_string(&with_slots).unwrap()).unwrap();
        assert_eq!(
            reparsed.node_instance.pairing_slots,
            with_slots.node_instance.pairing_slots
        );
        assert_eq!(reparsed.node_instance.copy, with_slots.node_instance.copy);
    }

    #[test]
    fn rejects_invalid_instance_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("peppy_launcher.json5");

        let err = runtime_config_from_json("bad id!")
            .and_then(|config| config.save_json5_launch_config(&path))
            .unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref msg)) if msg.contains("Invalid name"))
                || matches!(err, Error::Parsing(ParsingError::InvalidName(_, _))),
            "expected parsing error about invalid name, got: {err}"
        );
    }
}
