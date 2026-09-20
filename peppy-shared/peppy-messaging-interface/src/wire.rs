//! Transport-neutral addressing structs for peppy's messaging protocol.
//!
//! The schema (core_node / instance_id / target / topic|service|action / name)
//! is peppy-specific. The zenoh-shaped wire format that encodes it lives in
//! `wire::zenoh_format`.

use config::runtime::ProducerRef;
use std::fmt;

/// A validated keyexpr segment. The wire format builds keyexprs by joining
/// segments with `/`, so a segment must be non-empty, contain no `/`, and not
/// collide with the reserved sentinels (`*`, `**`, `_`) used by the wire format
/// for wildcard positions. `@` is also forbidden so the CLI's `--bind KEY@VALUE`
/// parser stays unambiguous.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Segment(String);

impl Segment {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reserved single-character segment placed at the `link_id` wire slot when
    /// a producer is run without `--link-id`. User-supplied input cannot
    /// construct this segment via `try_from` (the reserved-sentinel rule
    /// rejects `_`); the runtime uses this constructor to materialize the
    /// default sentinel at producer-side fan-out sites.
    pub fn default_link_id() -> Self {
        Self(DEFAULT_LINK_ID.to_string())
    }

    /// Lenient parser used by runtime paths that need to accept the reserved
    /// link_id default `_` alongside user-supplied link_ids (for example, when
    /// parsing the `PEPPY_LINK_IDS` env var inside the runner). Continues to
    /// reject `*` / `**` so a publisher can never advertise itself on a
    /// wildcard.
    pub fn try_link_id(s: &str) -> Result<Self, SegmentError> {
        validate_segment_chars(s)?;
        if matches!(s, "*" | "**") {
            return Err(SegmentError::ReservedSentinel(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    /// `Some(value)` → [`Self::try_link_id`]; `None` →
    /// [`Self::default_link_id`]. Used by wire constructors to map the
    /// optional consumer/producer `link_id` argument.
    pub fn link_id_or_default(value: Option<&str>) -> Result<Self, SegmentError> {
        match value {
            Some(s) => Self::try_link_id(s),
            None => Ok(Self::default_link_id()),
        }
    }

    /// Non-allocating equivalent of `Segment::try_from(s).is_ok()`, for wire
    /// parsers that classify observed keyexpr segments without needing an
    /// owned `Segment`.
    pub(crate) fn is_valid(s: &str) -> bool {
        validate_segment_chars(s).is_ok() && !is_reserved_sentinel(s)
    }
}

/// Wire literal used at the `link_id` slot when a producer is run without
/// `--link-id`. Kept in sync with [`Segment::default_link_id`] / the CLI
/// validator in `peppy::commands::node::run`. The single source of truth
/// lives in `config::consts::DEFAULT_LINK_ID_SENTINEL`; this is a re-export.
pub const DEFAULT_LINK_ID: &str = config::consts::DEFAULT_LINK_ID_SENTINEL;

impl std::ops::Deref for Segment {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Segment {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Segment {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl TryFrom<&str> for Segment {
    type Error = SegmentError;

    fn try_from(s: &str) -> Result<Self, SegmentError> {
        validate_segment_chars(s)?;
        if is_reserved_sentinel(s) {
            return Err(SegmentError::ReservedSentinel(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }
}

/// The full sentinel set rejected by [`Segment::try_from`] (unlike
/// [`Segment::try_link_id`], which admits the link_id default `_`).
fn is_reserved_sentinel(s: &str) -> bool {
    matches!(s, "*" | "**") || s == DEFAULT_LINK_ID
}

/// Shared char-level validation: non-empty, no `/`, no `@`. The reserved
/// sentinel check differs between [`Segment::try_link_id`] (rejects only
/// `*`/`**`) and [`Segment::try_from`] (also rejects `_`), so callers
/// apply that check separately.
fn validate_segment_chars(s: &str) -> Result<(), SegmentError> {
    if s.is_empty() {
        return Err(SegmentError::Empty);
    }
    if s.contains('/') {
        return Err(SegmentError::ContainsSlash(s.to_string()));
    }
    if s.contains('@') {
        return Err(SegmentError::ContainsAt(s.to_string()));
    }
    Ok(())
}

impl TryFrom<String> for Segment {
    type Error = SegmentError;

    fn try_from(s: String) -> Result<Self, SegmentError> {
        Self::try_from(s.as_str())
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returned by [`Segment::try_from`] when a candidate string violates the
/// keyexpr-segment invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
    Empty,
    ContainsSlash(String),
    ContainsAt(String),
    ReservedSentinel(String),
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("keyexpr segment must not be empty"),
            Self::ContainsSlash(s) => {
                write!(f, "keyexpr segment '{s}' must not contain '/'")
            }
            Self::ContainsAt(s) => {
                write!(f, "keyexpr segment '{s}' must not contain '@'")
            }
            Self::ReservedSentinel(s) => {
                write!(f, "keyexpr segment '{s}' collides with a reserved sentinel")
            }
        }
    }
}

impl std::error::Error for SegmentError {}

/// Wire discriminator placed before the name/tag pair on senders whose target
/// is a contract (a `manifest.implements`-backed declaration).
pub(crate) const CONTRACT_DISCRIMINATOR: &str = "contract";

/// Wire discriminator placed before the name/tag pair on senders whose target
/// is a node (a native declaration, no `manifest.implements` slot).
pub(crate) const NODE_DISCRIMINATOR: &str = "node";

/// Wire discriminator placed before the name/tag pair on senders whose target
/// is a pairing (a `depends_on.pairings` slot). Distinct from `contract` so
/// pairing traffic can never match a contract subscription.
pub(crate) const PAIRING_DISCRIMINATOR: &str = "pairing";

fn validated_name_tag(name: &str, tag: &str) -> Result<(Segment, Segment), SenderTargetError> {
    let name_segment = Segment::try_from(name)?;
    // Hyphen-to-underscore tag normalization: the generator emits tags with
    // hyphens (config-side identifier rule); the wire form requires
    // identifier-safe segments. Shared with the config layer's parse-time
    // implements-collision check, which predicts this exact transformation.
    let normalized_tag = config::consts::normalize_tag(tag);
    let tag_segment = Segment::try_from(normalized_tag.as_str())?;
    Ok((name_segment, tag_segment))
}

/// Identifier of a contract declared via `manifest.implements`. Carries the
/// contract's name and tag. Used as one variant of [`SenderTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContractIdentifier {
    contract_name: Segment,
    contract_tag: Segment,
}

impl ContractIdentifier {
    pub fn new(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        let (contract_name, contract_tag) = validated_name_tag(name, tag)?;
        Ok(Self {
            contract_name,
            contract_tag,
        })
    }

    pub fn name(&self) -> &str {
        self.contract_name.as_str()
    }

    pub fn tag(&self) -> &str {
        self.contract_tag.as_str()
    }
}

/// Identifier of a pairing declared via `depends_on.pairings`. Carries the
/// pairing's name and tag. Used as one variant of [`SenderTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PairingIdentifier {
    pairing_name: Segment,
    pairing_tag: Segment,
}

impl PairingIdentifier {
    pub fn new(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        let (pairing_name, pairing_tag) = validated_name_tag(name, tag)?;
        Ok(Self {
            pairing_name,
            pairing_tag,
        })
    }

    pub fn name(&self) -> &str {
        self.pairing_name.as_str()
    }

    pub fn tag(&self) -> &str {
        self.pairing_tag.as_str()
    }
}

/// Identifier of a node (a native declaration). Carries the node's name and
/// tag (from `manifest.tag`). Used as one variant of [`SenderTarget`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeIdentifier {
    node_name: Segment,
    node_tag: Segment,
}

impl NodeIdentifier {
    pub fn new(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        let (node_name, node_tag) = validated_name_tag(name, tag)?;
        Ok(Self {
            node_name,
            node_tag,
        })
    }

    /// Builds a node identifier from segments the caller has already validated
    /// upstream (e.g. via `config::Name`). Panics if a segment turns out to
    /// collide with a reserved wire sentinel; the only inputs that can trigger
    /// this are degenerate `Name`s like `"_"` or `"-"` (the latter after
    /// hyphen-to-underscore tag normalization). Use this at call sites whose
    /// inputs were funneled through a typed `Name` boundary.
    pub fn from_validated(name: &str, tag: &str) -> Self {
        Self::new(name, tag).expect("validated name and tag should be wire-segment safe")
    }

    pub fn name(&self) -> &str {
        self.node_name.as_str()
    }

    pub fn tag(&self) -> &str {
        self.node_tag.as_str()
    }
}

/// Addressing target carried by a sender (or matched by a receiver). Each
/// emission is **either** a contract, a node, **or** a pairing — never more
/// than one. The wire format embeds a `contract`|`node`|`pairing`
/// discriminator so the three identifier spaces cannot collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SenderTarget {
    Contract(ContractIdentifier),
    Node(NodeIdentifier),
    Pairing(PairingIdentifier),
}

impl SenderTarget {
    /// Shortcut: `SenderTarget::contract("manipulator", "v1")` instead of
    /// `SenderTarget::Contract(ContractIdentifier::new("manipulator", "v1")?)`.
    pub fn contract(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        ContractIdentifier::new(name, tag).map(Self::Contract)
    }

    /// Shortcut: `SenderTarget::node("uvc_camera", "v1")` instead of
    /// `SenderTarget::Node(NodeIdentifier::new("uvc_camera", "v1")?)`.
    pub fn node(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        NodeIdentifier::new(name, tag).map(Self::Node)
    }

    /// Builds a node-shaped target from segments validated upstream (e.g. via
    /// `config::Name`). See [`NodeIdentifier::from_validated`] for the panic
    /// contract.
    pub fn node_from_validated(name: &str, tag: &str) -> Self {
        Self::Node(NodeIdentifier::from_validated(name, tag))
    }

    /// Shortcut: `SenderTarget::pairing("arm_link", "v1")` instead of
    /// `SenderTarget::Pairing(PairingIdentifier::new("arm_link", "v1")?)`.
    pub fn pairing(name: &str, tag: &str) -> Result<Self, SenderTargetError> {
        PairingIdentifier::new(name, tag).map(Self::Pairing)
    }

    pub(crate) fn discriminator(&self) -> &'static str {
        match self {
            Self::Contract(_) => CONTRACT_DISCRIMINATOR,
            Self::Node(_) => NODE_DISCRIMINATOR,
            Self::Pairing(_) => PAIRING_DISCRIMINATOR,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Contract(c) => c.name(),
            Self::Node(n) => n.name(),
            Self::Pairing(p) => p.name(),
        }
    }

    pub fn tag(&self) -> &str {
        match self {
            Self::Contract(c) => c.tag(),
            Self::Node(n) => n.tag(),
            Self::Pairing(p) => p.tag(),
        }
    }

    pub fn is_contract(&self) -> bool {
        matches!(self, Self::Contract(_))
    }

    pub fn is_node(&self) -> bool {
        matches!(self, Self::Node(_))
    }

    pub fn is_pairing(&self) -> bool {
        matches!(self, Self::Pairing(_))
    }
}

/// Returned by [`ContractIdentifier::new`] / [`NodeIdentifier::new`] /
/// [`SenderTarget::contract`] / [`SenderTarget::node`] when a name or tag
/// segment fails validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderTargetError {
    InvalidSegment(SegmentError),
}

impl From<SegmentError> for SenderTargetError {
    fn from(err: SegmentError) -> Self {
        Self::InvalidSegment(err)
    }
}

impl fmt::Display for SenderTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSegment(err) => write!(f, "invalid sender target segment: {err}"),
        }
    }
}

impl std::error::Error for SenderTargetError {}

/// Discriminator for service-shaped traffic. Replaces the stringly-typed
/// `message_type: &str` (`"service"` / `"action"`) parameter previously
/// threaded through call sites.
///
/// On the wire, `Service` produces `service/{discriminator}/.../{name}` while
/// action variants produce `action/{discriminator}/.../{name}/{goal|cancel|result}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    Service,
    ActionGoal,
    ActionCancel,
    ActionResult,
}

impl ServiceKind {
    /// First segment of the service root (`"service"` or `"action"`).
    pub fn root_segment(self) -> &'static str {
        match self {
            ServiceKind::Service => "service",
            ServiceKind::ActionGoal | ServiceKind::ActionCancel | ServiceKind::ActionResult => {
                "action"
            }
        }
    }

    /// Trailing segment appended after the service name for action sub-services,
    /// or `None` for a plain service.
    pub fn suffix(self) -> Option<&'static str> {
        match self {
            ServiceKind::Service => None,
            ServiceKind::ActionGoal => Some("goal"),
            ServiceKind::ActionCancel => Some("cancel"),
            ServiceKind::ActionResult => Some("result"),
        }
    }
}

/// The precomputed `[root, discriminator, name, tag]` prefix segments of a
/// service_root. Built once at receiver construction so the inbound-query
/// parser can match without rebuilding strings. Single source of truth for the
/// prefix shape, shared by [`ServiceWireReceiver::new`] and the action
/// sub-service derivation in [`ActionWireReceiver`].
fn service_root_prefix(identity: &SenderTarget, kind: ServiceKind) -> [String; 4] {
    [
        kind.root_segment().to_string(),
        identity.discriminator().to_string(),
        identity.name().to_string(),
        identity.tag().to_string(),
    ]
}

// ─── Topics ──────────────────────────────────────────────────────────────────

/// One end of a pair as a pairing topic key names it: the instance's wire
/// address and the link_id of its slot in the pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WirePeer {
    pub(crate) core_node: Segment,
    pub(crate) instance_id: Segment,
    pub(crate) link_id: Segment,
}

impl WirePeer {
    pub fn new(core_node: &str, instance_id: &str, link_id: &str) -> crate::error::Result<Self> {
        Ok(Self {
            core_node: Segment::try_from(core_node)?,
            instance_id: Segment::try_from(instance_id)?,
            link_id: Segment::try_link_id(link_id)?,
        })
    }
}

/// Publisher-side addressing for a topic emit. Fields are `pub(crate)` so
/// external callers go through the validating constructors; the wire format
/// and adapter code inside this crate can read fields directly.
///
/// A contract or node emission is addressed to whoever subscribes. A pairing
/// emission from a slot holding several pairs names the peer it is for, so
/// each peer hears its own alone; one from a scalar slot is addressed to the
/// one peer that holds its pair, whoever that is. [`Self::new`] builds the
/// first and refuses a pairing target, [`Self::to_peer`] the second,
/// [`Self::to_sole_peer`] the third.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_target: SenderTarget,
    pub(crate) link_id: Segment,
    pub(crate) as_topic_name: Segment,
    /// The peer a multi slot's pairing emission is for; `None` on a scalar
    /// slot's emission and on every other target.
    pub(crate) to_peer: Option<WirePeer>,
}

impl TopicWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        as_topic_name: &str,
    ) -> crate::error::Result<Self> {
        if as_target.is_pairing() {
            return Err(crate::error::Error::PairingPublishNamesNoPeer);
        }
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_target,
            link_id: Segment::link_id_or_default(link_id)?,
            as_topic_name: Segment::try_from(as_topic_name)?,
            to_peer: None,
        })
    }

    /// A pairing emission from this node's slot `link_id` to `peer`.
    pub fn to_peer(
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: &str,
        as_topic_name: &str,
        peer: WirePeer,
    ) -> crate::error::Result<Self> {
        if !as_target.is_pairing() {
            return Err(crate::error::Error::PeerOnNonPairingPublish);
        }
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_target,
            link_id: Segment::try_link_id(link_id)?,
            as_topic_name: Segment::try_from(as_topic_name)?,
            to_peer: Some(peer),
        })
    }

    /// A pairing emission from this node's scalar slot `link_id` to the one
    /// peer holding its pair: the recipient stays open, and the peer's own
    /// pinned subscription is what selects it.
    pub fn to_sole_peer(
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: &str,
        as_topic_name: &str,
    ) -> crate::error::Result<Self> {
        if !as_target.is_pairing() {
            return Err(crate::error::Error::PeerOnNonPairingPublish);
        }
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_target,
            link_id: Segment::try_link_id(link_id)?,
            as_topic_name: Segment::try_from(as_topic_name)?,
            to_peer: None,
        })
    }
}

/// Which recipient a pairing subscription stands for. A pairing emission
/// names its peer, so a subscription to one either is that peer, and pins
/// its own slot, or observes the pairing and matches the emissions to every peer, or
/// observes it pinned to the one peer of the pair it was named by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingRecipient {
    /// This node's own slot in the pair.
    Slot(Segment),
    /// Any peer: the observer shape.
    Any,
    /// One named peer of the source's slot: the observer shape pinned to a
    /// pair.
    Peer(WirePeer),
}

/// Subscriber-side addressing for a topic. `from_core_node` / `from_instance_id` /
/// `from_target` identify the publisher whose messages we want to receive;
/// `None` means "any" (translated to the transport's single-chunk wildcard).
/// A consumer slot bound to a single producer pins both `from_core_node` and
/// `from_instance_id`; a slot bound to several producers subscribes with both
/// slots wildcarded and peppylib's `Subscription` wrapper filters incoming
/// messages against the bound producer set above the adapter.
/// `from_link_id` follows the same rule: `Some` pins to a producer's specific
/// link_id (pairing subscriptions), `None` matches any.
///
/// A pairing subscription also says which recipient it stands for
/// ([`PairingRecipient`]), because a pairing emission names its peer:
/// [`Self::paired`] pins this node's own slot, [`Self::observing`] matches
/// the emissions to every peer, [`Self::observing_pair`] pins one named peer, and
/// [`Self::new`] refuses a pairing target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicWireReceiver {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) from_core_node: Option<Segment>,
    pub(crate) from_instance_id: Option<Segment>,
    pub(crate) from_target: Option<SenderTarget>,
    pub(crate) from_link_id: Option<Segment>,
    pub(crate) to_topic: Segment,
    /// Which recipient a pairing subscription stands for; `None` on every
    /// other target.
    pub(crate) pairing_recipient: Option<PairingRecipient>,
}

impl TopicWireReceiver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        from_target: Option<SenderTarget>,
        from_link_id: Option<&str>,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        if from_target.as_ref().is_some_and(SenderTarget::is_pairing) {
            return Err(crate::error::Error::PairingSubscriptionNamesNoRecipient);
        }
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            from_core_node: from_core_node.map(Segment::try_from).transpose()?,
            from_instance_id: from_instance_id.map(Segment::try_from).transpose()?,
            from_target,
            from_link_id: from_link_id.map(Segment::try_link_id).transpose()?,
            to_topic: Segment::try_from(to_topic)?,
            pairing_recipient: None,
        })
    }

    /// This node's slot `own_link_id` receiving what `peer` publishes on the
    /// pair between them: pinned on both ends.
    pub fn paired(
        as_core_node: &str,
        as_instance_id: &str,
        pairing: SenderTarget,
        peer: &WirePeer,
        own_link_id: &str,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Self::pairing_recipient(
            as_core_node,
            as_instance_id,
            pairing,
            peer,
            PairingRecipient::Slot(Segment::try_link_id(own_link_id)?),
            to_topic,
        )
    }

    /// This node observing what `source` publishes on its pairing slot, to
    /// whichever peer: pinned on the source, open on the recipient.
    pub fn observing(
        as_core_node: &str,
        as_instance_id: &str,
        pairing: SenderTarget,
        source: &WirePeer,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Self::pairing_recipient(
            as_core_node,
            as_instance_id,
            pairing,
            source,
            PairingRecipient::Any,
            to_topic,
        )
    }

    /// This node observing what `source` publishes to `peer` on the pair
    /// between them: pinned on both ends, by a third party.
    pub fn observing_pair(
        as_core_node: &str,
        as_instance_id: &str,
        pairing: SenderTarget,
        source: &WirePeer,
        peer: &WirePeer,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        Self::pairing_recipient(
            as_core_node,
            as_instance_id,
            pairing,
            source,
            PairingRecipient::Peer(peer.clone()),
            to_topic,
        )
    }

    fn pairing_recipient(
        as_core_node: &str,
        as_instance_id: &str,
        pairing: SenderTarget,
        producer: &WirePeer,
        recipient: PairingRecipient,
        to_topic: &str,
    ) -> crate::error::Result<Self> {
        if !pairing.is_pairing() {
            return Err(crate::error::Error::RecipientOnNonPairingSubscription);
        }
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            from_core_node: Some(producer.core_node.clone()),
            from_instance_id: Some(producer.instance_id.clone()),
            from_target: Some(pairing),
            from_link_id: Some(producer.link_id.clone()),
            to_topic: Segment::try_from(to_topic)?,
            pairing_recipient: Some(recipient),
        })
    }

    /// Wire rule shared by every adapter: wildcard-link_id subscribers
    /// (`from_link_id: None`) match every per-link_id publish a multi-link
    /// `emit` produces and must drop the secondaries — see the
    /// topic-attachment block in `wire::zenoh_format`. Pinned subscribers
    /// ignore the attachment because their keyexpr already selects a single
    /// publish per emit.
    pub fn drops_secondary_publishes(&self) -> bool {
        self.from_link_id.is_none()
    }
}

// ─── Services ────────────────────────────────────────────────────────────────

/// Caller-side addressing for a service. `target` is the producer's full
/// `(core_node, instance_id)` wire address; `None` is a genuine wildcard
/// on both slots (the discovery probe shape). The one representable
/// half-address is core-without-instance via [`Self::scoped_to_core_node`]
/// — for callers that know which core node must answer but cannot know
/// the producer's per-boot instance_id. Instance-without-core stays
/// unrepresentable at this boundary.
/// The link_id wire slot is always emitted as `*` — producers advertise under
/// the reserved `_` segment, and Zenoh's matcher unifies the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireSender {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) target_core_node: Option<Segment>,
    pub(crate) target_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_service_name: Segment,
    pub(crate) kind: ServiceKind,
}

impl ServiceWireSender {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        target: Option<&ProducerRef>,
        to_target: SenderTarget,
        to_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            target_core_node: target
                .map(|t| Segment::try_from(t.core_node.as_str()))
                .transpose()?,
            target_instance_id: target
                .map(|t| Segment::try_from(t.instance_id.as_str()))
                .transpose()?,
            to_target,
            to_service_name: Segment::try_from(to_service_name)?,
            kind,
        })
    }

    /// Scopes the selector's target core_node slot to `target_core_node`
    /// while leaving the instance slot wildcarded — the core-without-instance
    /// half-address. The selector then matches only producers hosted by that
    /// core node, regardless of their per-boot instance_id.
    pub fn scoped_to_core_node(mut self, target_core_node: &str) -> crate::error::Result<Self> {
        self.target_core_node = Some(Segment::try_from(target_core_node)?);
        self.target_instance_id = None;
        Ok(self)
    }

    pub fn to_service_name(&self) -> &str {
        &self.to_service_name
    }

    pub fn target_instance_id(&self) -> Option<&str> {
        self.target_instance_id.as_deref()
    }
}

/// Server-side addressing for a service. Producers always advertise their
/// queryables under the reserved default `_` segment at the link_id wire
/// slot; inbound queries carry `*` (callers always wildcard the link_id
/// slot) or the `_` literal, and the dispatch filter at the adapter
/// accepts both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceWireReceiver {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_identity: SenderTarget,
    pub(crate) as_service_name: Segment,
    pub(crate) kind: ServiceKind,
    /// Precomputed `[root, discriminator, name, tag]` segments of the
    /// service_root prefix. Derived from `(as_identity, kind)` at construction
    /// so the inbound query parser can match without rebuilding strings.
    service_root_prefix: [String; 4],
}

impl ServiceWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_service_name: &str,
        kind: ServiceKind,
    ) -> crate::error::Result<Self> {
        let service_root_prefix = service_root_prefix(&as_identity, kind);
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_identity,
            as_service_name: Segment::try_from(as_service_name)?,
            kind,
            service_root_prefix,
        })
    }

    pub(crate) fn service_root_prefix_segments(&self) -> &[String; 4] {
        &self.service_root_prefix
    }
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Caller-side addressing for an action. Goal / cancel / result are exposed
/// as derived [`ServiceWireSender`]s with the appropriate [`ServiceKind`].
/// Feedback subscription is built per `goal_id` by the transport adapter.
/// `target` is the producer's full `(core_node, instance_id)` wire address;
/// `None` is a genuine wildcard on both slots (the discovery probe shape).
/// The link_id wire slot is always `*` — producers advertise under the
/// reserved `_` segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireSender {
    pub(crate) as_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) target_core_node: Option<Segment>,
    pub(crate) target_instance_id: Option<Segment>,
    pub(crate) to_target: SenderTarget,
    pub(crate) to_action_name: Segment,
}

impl ActionWireSender {
    pub fn new(
        as_core_node: &str,
        as_instance_id: &str,
        target: Option<&ProducerRef>,
        to_target: SenderTarget,
        to_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            as_core_node: Segment::try_from(as_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            target_core_node: target
                .map(|t| Segment::try_from(t.core_node.as_str()))
                .transpose()?,
            target_instance_id: target
                .map(|t| Segment::try_from(t.instance_id.as_str()))
                .transpose()?,
            to_target,
            to_action_name: Segment::try_from(to_action_name)?,
        })
    }

    pub fn goal_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionGoal)
    }

    pub fn cancel_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionCancel)
    }

    pub fn result_service(&self) -> ServiceWireSender {
        self.action_service(ServiceKind::ActionResult)
    }

    pub fn to_action_name(&self) -> &str {
        &self.to_action_name
    }

    pub fn target_core_node(&self) -> Option<&str> {
        self.target_core_node.as_deref()
    }

    pub fn target_instance_id(&self) -> Option<&str> {
        self.target_instance_id.as_deref()
    }

    /// Returns a clone with `target_core_node` and `target_instance_id`
    /// overwritten by the given values. Used by `ActionMessenger::send_goal`
    /// to latch the stored sender to the responding producer after the first
    /// `goal_response` arrives, so cancel / result / feedback all target the
    /// winner instead of fanning out to every producer that received the
    /// wildcard goal.
    pub fn pinned_to(&self, core_node: &str, instance_id: &str) -> crate::error::Result<Self> {
        let mut out = self.clone();
        out.target_core_node = Some(Segment::try_from(core_node)?);
        out.target_instance_id = Some(Segment::try_from(instance_id)?);
        Ok(out)
    }

    fn action_service(&self, kind: ServiceKind) -> ServiceWireSender {
        ServiceWireSender {
            bound_core_node: self.as_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            target_core_node: self.target_core_node.clone(),
            target_instance_id: self.target_instance_id.clone(),
            to_target: self.to_target.clone(),
            to_service_name: self.to_action_name.clone(),
            kind,
        }
    }
}

/// Server-side addressing for an action. Producers always advertise under the
/// reserved default `_` link_id segment; the adapter accepts `*` or `_` at the
/// link_id wire slot. Per-goal feedback publishes use the goal's own link_id
/// (extracted from the goal request).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionWireReceiver {
    pub(crate) bound_core_node: Segment,
    pub(crate) as_instance_id: Segment,
    pub(crate) as_identity: SenderTarget,
    pub(crate) as_action_name: Segment,
}

impl ActionWireReceiver {
    pub fn new(
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_action_name: &str,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            bound_core_node: Segment::try_from(bound_core_node)?,
            as_instance_id: Segment::try_from(as_instance_id)?,
            as_identity,
            as_action_name: Segment::try_from(as_action_name)?,
        })
    }

    pub fn goal_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionGoal)
    }

    pub fn cancel_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionCancel)
    }

    pub fn result_service(&self) -> ServiceWireReceiver {
        self.action_service(ServiceKind::ActionResult)
    }

    fn action_service(&self, kind: ServiceKind) -> ServiceWireReceiver {
        let service_root_prefix = service_root_prefix(&self.as_identity, kind);
        ServiceWireReceiver {
            bound_core_node: self.bound_core_node.clone(),
            as_instance_id: self.as_instance_id.clone(),
            as_identity: self.as_identity.clone(),
            as_service_name: self.as_action_name.clone(),
            kind,
            service_root_prefix,
        }
    }
}

pub(crate) mod zenoh_format;

/// Public channel-address templates, re-exported at the crate root as
/// `pmi::templates`. Pure string code — compiled in every feature config.
pub mod templates;

pub use zenoh_format::{ServiceQueryKind, ServiceReplyKind};

#[cfg(test)]
mod tests;
