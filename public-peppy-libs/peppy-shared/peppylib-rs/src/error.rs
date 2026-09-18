use std::env::VarError;
use std::fmt;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during parameter deserialization or validation.
#[derive(Debug)]
pub struct ParameterDeserializationError(pub Vec<String>);

impl ParameterDeserializationError {
    pub fn single(message: impl Into<String>) -> Self {
        Self(vec![message.into()])
    }

    pub fn multiple(messages: Vec<String>) -> Self {
        Self(messages)
    }
}

impl fmt::Display for ParameterDeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [] => write!(f, "parameter deserialization error: unknown error"),
            [single] => write!(f, "parameter deserialization error: {}", single),
            multiple => {
                write!(f, "missing required parameters:")?;
                for msg in multiple {
                    write!(f, "\n  - {}", msg)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ParameterDeserializationError {}

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A failure raised by the node's own logic rather than by the runtime: a
    /// configuration it refuses, a precondition it cannot meet. Never
    /// constructed by the runtime.
    ///
    /// Not `#[from]`, so `?` cannot absorb an arbitrary boxed error into this
    /// type; `#[source]` rather than transparent, so the wrapped error stays
    /// reachable through [`std::error::Error::source`].
    #[error("{0}")]
    Node(#[source] Box<dyn std::error::Error + Send + Sync>),

    // -- system clock (set before the Unix epoch); produced by `clock::wall_now_ns`
    #[error("system clock unavailable: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),

    // -- config
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    // -- serde
    #[error(transparent)]
    SerdeJson5(#[from] serde_json5::Error),

    // -- peppy-messaging-interface
    #[error(transparent)]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),

    // -- wire-format input validation (SenderTarget construction in generated code)
    #[error(transparent)]
    InvalidSenderTarget(#[from] pmi::SenderTargetError),

    // -- core-node-api
    #[error(transparent)]
    CoreNodeApi(#[from] core_node_api::Error),

    #[error("invalid service request '{identifier}': {reason}")]
    InvalidServiceRequest { identifier: String, reason: String },

    #[error("clock not ready: this instance's clock domain has published no tick yet")]
    ClockNotReady,

    #[error("internal encoding error for '{identifier}': {reason}")]
    InternalEncodingError { identifier: String, reason: String },

    #[error("service request stream closed unexpectedly")]
    ServiceRequestStreamClosed,

    #[error("action feedback channel closed unexpectedly")]
    ActionFeedbackChannelClosed,

    // -- pairing
    #[error(
        "unknown pairing slot '{link_id}': the manifest declares no depends_on.pairings entry with that link_id"
    )]
    UnknownPairingSlot { link_id: String },

    #[error("pairing slot channel closed (runtime torn down while waiting for a peer)")]
    PairingSlotClosed,

    #[error("pairing slot `{link_id}` holds no pair with {peer}, so nothing is published to it")]
    PeerNotPaired { link_id: String, peer: String },

    #[error(
        "pairing slot `{link_id}` is seeded with {count} pairs, but its cardinality `{cardinality}` holds at most one: declare `one_or_more` or `zero_or_more` on the slot, or seed one pair"
    )]
    PairingSeedNotScalar {
        link_id: String,
        cardinality: String,
        count: usize,
    },

    /// One pair appears twice in a pairing slot's seed. A slot holds each pair
    /// once, in its seed as in every live delivery.
    #[error(
        "pairing slot `{link_id}` seeds the pair with peer `{core_node}/{instance_id}` (slot `{peer_link_id}`) twice; a slot holds each pair once"
    )]
    PairingSeedDuplicate {
        link_id: String,
        core_node: String,
        instance_id: String,
        peer_link_id: String,
    },

    /// The boot config seeds a pairing slot the manifest does not declare:
    /// component version skew (stale codegen, or a daemon working from a
    /// different manifest), never a user error.
    #[error(
        "boot config seeds pairing slot `{link_id}`, which this node's manifest does not declare"
    )]
    PairingSeedUndeclared { link_id: String },

    #[error("copy name `{copy}` on the pin of pairing slot `{link_id}` is not a name: {reason}")]
    InvalidCopyName {
        link_id: String,
        copy: String,
        reason: String,
    },

    // -- observation (pairing observers)
    #[error(
        "unknown observer slot '{link_id}': the manifest declares no depends_on.pairing_observers entry with that link_id"
    )]
    UnknownObservationSlot { link_id: String },

    // -- endpoints (`execution.endpoints`)
    #[error(
        "endpoint `{label}` is not declared: the manifest's `execution.endpoints` names no such label, so nothing can be announced under it"
    )]
    UndeclaredEndpoint { label: String },

    #[error(
        "endpoint `{label}` is already announced; a label is announced once, with the socket the node bound"
    )]
    EndpointAlreadyAnnounced { label: String },

    #[error(
        "endpoint `{label}` cannot be announced after setup returned: the announced set is sealed when `setup_fn` returns, so every announcement belongs inside setup"
    )]
    EndpointsSealed { label: String },

    #[error("endpoint `{label}` has an invalid binding: {reason}")]
    InvalidEndpointBinding { label: String, reason: String },

    #[error(
        "endpoint `{label}` is declared in the manifest's `execution.endpoints` but setup returned without announcing it: call `announce_endpoint` with the socket the node bound"
    )]
    EndpointNotAnnounced { label: String },

    // -- topics/services/actions errors
    #[error(
        "service '{service_name}'{instance_suffix} is unreachable",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceUnreachable {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "service '{service_name}'{instance_suffix} has timed out",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceTimeout {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "service '{service_name}'{instance_suffix} returned error: {reason}",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceError {
        instance_id: Option<String>,
        service_name: String,
        reason: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} has timed out waiting for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultTimeout {
        instance_id: Option<String>,
        action_name: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} is unreachable for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultUnreachable {
        instance_id: Option<String>,
        action_name: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} producer disappeared before closing the feedback stream",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionFeedbackProducerGone {
        instance_id: Option<String>,
        action_name: String,
    },
    /// Distinct from a transport failure on purpose: the worker was reached and
    /// its goal is terminal, it simply never produced a result. Lumping this in
    /// with "failed to get result" would report the same thing for a worker that
    /// died and for one whose result merely aged out.
    #[error(
        "action '{action_name}'{instance_suffix} was abandoned by its worker before producing a result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionGoalAbandoned {
        instance_id: Option<String>,
        action_name: String,
    },
    /// See [`Error::ActionGoalAbandoned`]: the goal did finish, but its result
    /// was retained only for a bounded window that has since elapsed.
    #[error(
        "action '{action_name}'{instance_suffix} result expired before it could be fetched",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultExpired {
        instance_id: Option<String>,
        action_name: String,
    },

    // -- system
    #[error("failed to read `{var}` from the environment")]
    MissingInstanceIdEnvVar {
        var: &'static str,
        #[source]
        source: VarError,
    },

    #[error("failed to read launch config at `{path}`")]
    LaunchConfigRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse launch config at `{path}`")]
    LaunchConfigParse {
        path: String,
        #[source]
        source: serde_json5::Error,
    },

    #[error(
        "peppy config fingerprint mismatch for `{path}` (expected `{expected}`, got `{actual}`)"
    )]
    PeppyConfigFingerprintMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("failed to read codegen fingerprint at `{path}`")]
    CodegenFingerprintRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    ParameterTypeMismatch(#[from] config::TypeMismatch),

    #[error(transparent)]
    NodeArgumentsValidation(#[from] config::NodeArgumentsError),

    #[error(transparent)]
    ParameterDeserialization(#[from] ParameterDeserializationError),

    #[error("parameters have already been taken (take_parameters() can only be called once)")]
    ParametersAlreadyTaken,

    // --- Serialization
    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("invalid messenger configuration: {0}")]
    ConfigurationError(String),

    // --- Runner
    #[error("failed to build blocking runtime for `{context}`")]
    RuntimeInitialization {
        context: String,
        #[source]
        source: std::io::Error,
    },

    // --- Node/Topic
    #[error("invalid node name `{node_name}`: {reason}")]
    InvalidNodeName { node_name: String, reason: String },

    #[error("invalid core node name `{node_name}`: {reason}")]
    InvalidCoreNodeName { node_name: String, reason: String },

    #[error("failed to subscribe to topic `{topic_name}` in node `{node_name}`, {source_msg}")]
    TopicSubscribe {
        topic_name: String,
        node_name: String,
        source_msg: String,
    },

    #[error("subscription to `{topic_name}` closed without yielding a message")]
    SubscriptionClosed { topic_name: String },

    /// Startup backstop for the launch-time rule "every declared
    /// depends_on slot except `zero_or_more` carries a `slot_bindings`
    /// entry": a daemon that validates bindings never ships a boot config
    /// missing such a slot's entry, so hitting this means version skew or
    /// a hand-edited boot config. A `zero_or_one` slot the deployment wrote
    /// vacant is not missing: it arrives as an explicit empty set.
    #[error(
        "consumer slot `{link_id}` (cardinality `{cardinality}`) is unbound: the boot config \
         carries no producer set for it, but only a `zero_or_more` slot may be left unbound. \
         Fix the launcher / daemon that produced the boot config (or, in standalone mode, \
         seed the slot via `StandaloneConfig::with_bound_producer`, or \
         `with_vacant_producer_slot` for a `zero_or_one` slot that runs empty)"
    )]
    SlotUnbound {
        link_id: String,
        cardinality: config::node::Cardinality,
    },

    /// Startup backstop for the launch-time cardinality size rules: the
    /// boot config carries a bound set whose size the slot's declared
    /// cardinality does not allow (for example two producers on a `one`
    /// slot, or an empty set on a `one_or_more` slot).
    #[error(
        "consumer slot `{link_id}` declares cardinality `{cardinality}` but the boot config \
         binds {bound} producer(s) — the launcher validator enforces this at plan time, so \
         this boot config was produced by an incompatible component version or edited by hand"
    )]
    SlotCardinalityViolated {
        link_id: String,
        cardinality: config::node::Cardinality,
        bound: usize,
    },

    /// Startup backstop for observer slots, the observation twin of
    /// [`Self::SlotUnbound`] / [`Self::SlotCardinalityViolated`]: the boot
    /// config's seed for a slot has a size its declared cardinality does not
    /// allow. A missing seed counts as empty, so a `one` / `one_or_more`
    /// slot under a daemon that predates observation seeding fails here,
    /// loudly, instead of booting an empty slot the plan says cannot be.
    #[error(
        "observer slot `{link_id}` declares cardinality `{cardinality}` but the boot config \
         seeds {seeded} member(s) — the launcher validator enforces this at plan time, so \
         this boot config was produced by an incompatible component version (a daemon that \
         predates observation seeding, or an edited config)"
    )]
    ObservationSeedViolated {
        link_id: String,
        cardinality: config::node::Cardinality,
        seeded: usize,
    },

    /// One observed pairing appears twice in a slot's seed. The wire decoder
    /// rejects the same set on a live delivery, so accepting it here would
    /// let a hand-edited boot config open duplicate subscriptions.
    #[error(
        "observer slot `{link_id}` seeds the observed pairing \
         `{instance_id}/{source_link_id}@{core_node}` more than once — an observed pairing \
         may appear only once in a slot's member set"
    )]
    ObservationSeedDuplicate {
        link_id: String,
        core_node: String,
        instance_id: String,
        source_link_id: String,
    },

    /// The boot config seeds an observer slot the manifest does not
    /// declare: component version skew (stale codegen or a daemon working
    /// from a different manifest), never a user error.
    #[error(
        "the boot config seeds observer slot `{link_id}`, which this node's manifest does \
         not declare — the daemon and this node disagree about the manifest (version skew \
         or stale codegen); re-sync and rebuild the node"
    )]
    ObservationSeedUndeclared { link_id: String },

    /// A directed service / action call named a producer outside the
    /// slot's bound set. Every `poll` / `fire_goal` target must come from
    /// the slot's own bound set, exposed by the slot's cardinality-typed
    /// accessor (`bound_producer()` for the scalar cardinalities,
    /// `bound_producers()` for the multi ones): an out-of-set instance was
    /// never checked by
    /// plan-time binding validation, and membership is per slot, so a
    /// producer bound to a different slot of the same consumer is rejected
    /// all the same.
    #[error(
        "target `{instance_id}@{core_node}` is not in the bound set of consumer slot \
         `{link_id}`: pass a producer from this slot's own generated accessor \
         (`bound_producer()` / `bound_producers()`)"
    )]
    TargetNotBound {
        link_id: String,
        core_node: String,
        instance_id: String,
    },

    #[error("message format for `{context}` is not available in the generator")]
    MessageFormatUnavailable { context: String },
}

struct InstanceSuffix<'a>(Option<&'a str>);

impl fmt::Display for InstanceSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(instance_id) = self.0 {
            write!(f, " for instance '{instance_id}'")
        } else {
            Ok(())
        }
    }
}
