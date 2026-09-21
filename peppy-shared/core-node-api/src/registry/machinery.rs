//! Descriptor types and the macros behind the method registry.
//!
//! Everything here is plumbing for the [`methods!`] invocation in
//! [`crate::registry`] — the manifest lives there; this module only defines
//! the vocabulary ([`MethodKind`], [`Host`], [`PayloadDescriptor`],
//! [`Payloads`], [`MethodDescriptor`]) and the [`pd!`] / [`methods!`] macros
//! that expand it.

use core::any::TypeId;

use capnp::introspect::Type;

/// The three interaction styles a core-node method can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MethodKind {
    /// A zenoh queryable: the caller `get`s and the daemon replies.
    Service,
    /// Goal / cancel / result queryables plus a per-goal feedback pub/sub topic.
    Action,
    /// A one-way `put` on a pub/sub topic.
    Topic,
}

/// Which peer hosts the queryable/publisher for a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Host {
    /// The core-node daemon, addressed as `node/{core_node_name}/core`.
    CoreNodeDaemon,
    /// Every spawned node instance, addressed as `node/{node_name}/{node_tag}`;
    /// the daemon is the caller.
    SpawnedNode,
}

/// Ties a service's request codec to its response codec and [`ServiceId`],
/// implemented by the `methods!` expansion for every daemon-hosted service
/// with default routing. Client transports (peppylib's generic
/// `transport::poll`) are bounded on this, so a request/response pairing
/// declared once in the registry is the only pairing a caller can express —
/// there is no second per-method list to keep in sync.
///
/// Not implemented for [`Host::SpawnedNode`] services (their queryable does
/// not live under the daemon's service root) nor for `routing: bespoke`
/// entries (their route cannot be pinned by daemon-root discovery alone; see
/// the `methods!` docs).
///
/// [`ServiceId`]: crate::registry::ServiceId
pub trait ServiceRequest {
    /// The response codec paired with this request in the registry.
    type Response;
    /// The service this request belongs to.
    const ID: crate::registry::ServiceId;
    /// Encode `self` as the service's request payload.
    fn encode_request(&self) -> crate::Result<crate::Payload>;
    /// Decode the service's response payload.
    fn decode_response(data: &[u8]) -> crate::Result<Self::Response>;
}

/// Ties an action's goal codec to its [`ActionId`], implemented by the
/// `methods!` expansion for every action. The counterpart of
/// [`ServiceRequest`] for streaming goals; only the goal payload is carried
/// (feedback/result decoding happens on the goal handle, past the send).
///
/// [`ActionId`]: crate::registry::ActionId
pub trait ActionGoal {
    /// The action this goal belongs to.
    const ID: crate::registry::ActionId;
    /// Encode `self` as the action's goal payload.
    fn encode_goal(&self) -> crate::Result<crate::Payload>;
}

/// Marks a goal or request that may belong to a federated launch, implemented
/// by the `methods!` expansion for every entry declared `scope: launch`.
///
/// Which methods are launch-scoped is a property of the protocol, not of one
/// daemon's handler, so it is declared once in the registry rather than
/// rediscovered at each call site. A daemon reserved for someone else's launch
/// admits a goal only when it carries that launch's id, and bounding the check
/// on this trait is what stops it from being handed a goal that has no launch
/// scope at all.
///
/// Declaring `scope: launch` on an entry whose codec has no
/// `launch_id: Option<String>` field does not compile.
pub trait LaunchScoped {
    /// The launch this goal belongs to, or `None` for a goal a user typed
    /// directly.
    fn launch_id(&self) -> Option<&str>;
}

/// Everything the registry knows about a single Cap'n Proto payload.
///
/// `PartialEq` is intentionally not derived: the struct holds function
/// pointers, whose comparison is a lint and is meaningless anyway. Tests
/// compare the individual fields.
#[derive(Debug, Clone, Copy)]
pub struct PayloadDescriptor {
    /// Human-facing name of the Rust codec struct, e.g. `"ClockRequest"`.
    pub rust_type: &'static str,
    /// `TypeId` of the codec struct (`core_node_api::encoding::{rust_type}`).
    /// Sanity-checked by the registry tests; kept public for downstream
    /// identity checks.
    pub rust_type_id: fn() -> TypeId,
    /// Runtime Cap'n Proto reflection handle for the payload's wire root.
    pub introspect: fn() -> Type,
    /// `.capnp` file that defines the payload's struct, e.g. `"clock.capnp"`.
    pub schema_file: &'static str,
}

/// The payloads of a method, grouped by interaction style.
#[derive(Debug, Clone, Copy)]
pub enum Payloads {
    /// Request/reply.
    Service {
        request: PayloadDescriptor,
        response: PayloadDescriptor,
    },
    /// Streaming: goal + its ack, per-goal feedback, and the terminal result.
    Action {
        goal: PayloadDescriptor,
        goal_response: PayloadDescriptor,
        feedback: PayloadDescriptor,
        result: PayloadDescriptor,
    },
    /// Fire-and-forget publish.
    Topic { message: PayloadDescriptor },
}

impl Payloads {
    /// All payload descriptors in a stable, style-defined order
    /// (request, response / goal, goal_response, feedback, result / message).
    pub fn descriptors(&self) -> impl Iterator<Item = &PayloadDescriptor> {
        let descriptors = match self {
            Payloads::Service { request, response } => [Some(request), Some(response), None, None],
            Payloads::Action {
                goal,
                goal_response,
                feedback,
                result,
            } => [
                Some(goal),
                Some(goal_response),
                Some(feedback),
                Some(result),
            ],
            Payloads::Topic { message } => [Some(message), None, None, None],
        };
        descriptors.into_iter().flatten()
    }
}

/// A single core-node wire method.
#[derive(Debug, Clone, Copy)]
pub struct MethodDescriptor {
    /// The wire name, as declared in the `methods!` invocation and pinned by
    /// the registry's exhaustive wire-pin tests.
    pub name: &'static str,
    /// The peer that hosts this method.
    pub host: Host,
    /// A one-line human summary, condensed from the codec's rustdoc.
    pub summary: &'static str,
    /// The Cap'n Proto payloads carried by this method.
    pub payloads: Payloads,
}

impl MethodDescriptor {
    /// The interaction style, derived from [`MethodDescriptor::payloads`].
    pub fn kind(&self) -> MethodKind {
        match self.payloads {
            Payloads::Service { .. } => MethodKind::Service,
            Payloads::Action { .. } => MethodKind::Action,
            Payloads::Topic { .. } => MethodKind::Topic,
        }
    }
}

/// Build a [`PayloadDescriptor`] from a codec-struct ident and the schema
/// file. `rust_type` is the ident stringified; `rust_type_id` keys on
/// `crate::encoding::{ident}` (the only public path to the codec struct, and
/// the same type `peppylib` observes); `introspect` reaches the Cap'n Proto
/// root through the codec's [`Wire`](crate::encoding::Wire) impl (declared
/// beside the codec in `encoding/`) and coerces the generated
/// `Introspect::introspect` associated fn to `fn() -> Type`. Paths are fully
/// qualified so the invocation site needs no supporting imports.
macro_rules! pd {
    ($enc:ident, $file:literal) => {
        PayloadDescriptor {
                            rust_type: stringify!($enc),
                            rust_type_id: || ::core::any::TypeId::of::<crate::encoding::$enc>(),
                            introspect: <<crate::encoding::$enc as crate::encoding::Wire>::Root
                                as ::capnp::introspect::Introspect>::introspect,
                            schema_file: $file,
                        }
    };
}

/// Emits the [`ServiceRequest`] impl for one `methods!` service entry — or
/// nothing, when the generic client transport cannot route the service:
/// [`Host::SpawnedNode`] queryables don't live under the daemon's service
/// root, and `routing: bespoke` entries need discovery scoping no generic
/// signature can express. The first rule (daemon host, no routing override)
/// is the only emitting one; everything else falls through to the empty rule.
macro_rules! service_request_impl {
    ((CoreNodeDaemon) () $svar:ident, $sreq:ident, $sresp:ident) => {
        impl crate::registry::ServiceRequest for crate::encoding::$sreq {
            type Response = crate::encoding::$sresp;
            const ID: crate::registry::ServiceId = crate::registry::ServiceId::$svar;
            fn encode_request(&self) -> crate::Result<crate::Payload> {
                self.encode()
            }
            fn decode_response(data: &[u8]) -> crate::Result<Self::Response> {
                crate::encoding::$sresp::decode(data)
            }
        }
    };
    (($shost:ident) ($($srouting:ident)?) $svar:ident, $sreq:ident, $sresp:ident) => {};
}

/// Emits the [`LaunchScoped`] impl for an entry declared `scope: launch`,
/// or nothing when the entry declares no scope. Unlike
/// [`service_request_impl!`] there is no catch-all rule: a misspelled scope
/// matches neither arm and fails to compile, rather than quietly emitting
/// nothing and leaving the action ungated.
macro_rules! launch_scoped_impl {
    ((launch) $agoal:ident) => {
        impl crate::registry::LaunchScoped for crate::encoding::$agoal {
            fn launch_id(&self) -> Option<&str> {
                self.launch_id.as_deref()
            }
        }
    };
    (() $agoal:ident) => {};
}

/// Expands the one declaration point for every core-node wire method (the
/// invocation in [`crate::registry`]).
///
/// Each `services` entry emits a [`ServiceId`](crate::registry::ServiceId)
/// variant, each `actions` entry an [`ActionId`](crate::registry::ActionId)
/// variant, each `topics` entry a [`TopicId`](crate::registry::TopicId)
/// variant (per-kind enums, so `clock` can be both a service and a topic),
/// and every entry emits its [`MethodDescriptor`] in
/// [`METHODS`](crate::registry::METHODS) — in declaration order (services,
/// then actions, then topics), which `descriptor()` relies on for indexing
/// and the AsyncAPI generator relies on for stable document ordering.
///
/// Per entry: `name` is the wire string (the compatibility contract, pinned
/// by the registry's wire-pin tests); `summary` is the one-line human
/// description fed to the AsyncAPI docs and re-emitted as the variant's first
/// rustdoc line; doc comments on an entry become extended rustdoc on the
/// variant. Services carry a `host`; actions and topics are always
/// daemon-hosted today. Payload lines name the codec struct in
/// [`crate::encoding`]; its Cap'n Proto `Owned` root comes from the codec's
/// [`Wire`](crate::encoding::Wire) impl (see [`pd!`]).
///
/// Each service entry also emits a [`ServiceRequest`] impl on its request
/// codec, and each action entry an [`ActionGoal`] impl on its goal codec —
/// the hooks generic client transports are bounded on, so a method declared
/// here is callable with no per-method wrapper anywhere. A service may opt
/// out with `routing: bespoke` (after `host`) when daemon-root discovery
/// alone cannot pin its route — `node_stop`'s listener may be hosted by a
/// per-instance node, so its discovery must additionally be scoped to the
/// hosting core node and its peppylib wrapper stays hand-written. Emission
/// is gated by [`service_request_impl!`].
///
/// The enums deliberately do **not** get `Display`/`From<..> for &str` impls:
/// `.name()` at the wire boundary is the only sanctioned way back to a string,
/// so stringly-typed plumbing cannot quietly reappear. They are also not
/// `#[non_exhaustive]` — downstream crates matching exhaustively (daemon
/// registration, peppylib coverage) and thus failing to compile when a method
/// is added is the entire point of this registry.
macro_rules! methods {
    (
        services {
            $( $(#[$smeta:meta])* $svar:ident {
                name: $sname:literal,
                host: $shost:ident,
                $( routing: $srouting:ident, )?
                $( scope: $sscope:ident, )?
                summary: $ssummary:literal,
                request: $sreq:ident,
                response: $sresp:ident,
                schema: $sschema:literal $(,)?
            } )+
        }
        actions {
            $( $(#[$ameta:meta])* $avar:ident {
                name: $aname:literal,
                $( scope: $ascope:ident, )?
                summary: $asummary:literal,
                goal: $agoal:ident,
                goal_response: $agoal_resp:ident,
                feedback: $afb:ident,
                result: $ares:ident,
                schema: $aschema:literal $(,)?
            } )+
        }
        topics {
            $( $(#[$tmeta:meta])* $tvar:ident {
                name: $tname:literal,
                summary: $tsummary:literal,
                message: $tmsg:ident,
                schema: $tschema:literal $(,)?
            } )+
        }
    ) => {
        /// Every core-node **service** (request/reply queryable), one variant
        /// per method. Generated by `methods!`; match exhaustively (no
        /// wildcard arm) wherever a per-service decision is made, so a new
        /// service is a compile error there until it is handled.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ServiceId {
            $( #[doc = $ssummary] #[doc = ""] $(#[$smeta])* $svar, )+
        }

        impl ServiceId {
            /// Every service, in registry (declaration) order.
            pub const ALL: &'static [Self] = &[ $( Self::$svar, )+ ];

            /// The wire name. The only sanctioned enum-to-string step, for use
            /// at the wire boundary.
            pub const fn name(self) -> &'static str {
                match self { $( Self::$svar => $sname, )+ }
            }

            /// The peer that hosts this service's queryable.
            pub const fn host(self) -> Host {
                match self { $( Self::$svar => Host::$shost, )+ }
            }

            /// This service's entry in [`METHODS`].
            pub fn descriptor(self) -> &'static MethodDescriptor {
                // Services lead METHODS in declaration order.
                &METHODS[self as usize]
            }
        }

        $( service_request_impl!(($shost) ($($srouting)?) $svar, $sreq, $sresp); )+
        $( launch_scoped_impl!(($($sscope)?) $sreq); )+

        /// Every core-node **action** (streaming goal/feedback/result), one
        /// variant per method. Generated by `methods!`; match exhaustively
        /// (no wildcard arm) wherever a per-action decision is made.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum ActionId {
            $( #[doc = $asummary] #[doc = ""] $(#[$ameta])* $avar, )+
        }

        impl ActionId {
            /// Every action, in registry (declaration) order.
            pub const ALL: &'static [Self] = &[ $( Self::$avar, )+ ];

            /// The wire name. The only sanctioned enum-to-string step, for use
            /// at the wire boundary.
            pub const fn name(self) -> &'static str {
                match self { $( Self::$avar => $aname, )+ }
            }

            /// This action's entry in [`METHODS`].
            pub fn descriptor(self) -> &'static MethodDescriptor {
                // Actions follow the services in METHODS.
                &METHODS[ServiceId::ALL.len() + self as usize]
            }
        }

        $( impl crate::registry::ActionGoal for crate::encoding::$agoal {
            const ID: crate::registry::ActionId = crate::registry::ActionId::$avar;
            fn encode_goal(&self) -> crate::Result<crate::Payload> {
                self.encode()
            }
        } )+

        $( launch_scoped_impl!(($($ascope)?) $agoal); )+

        /// Every core-node **topic** (one-way publish), one variant per
        /// method. Generated by `methods!`; match exhaustively (no wildcard
        /// arm) wherever a per-topic decision is made.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum TopicId {
            $( #[doc = $tsummary] #[doc = ""] $(#[$tmeta])* $tvar, )+
        }

        impl TopicId {
            /// Every topic, in registry (declaration) order.
            pub const ALL: &'static [Self] = &[ $( Self::$tvar, )+ ];

            /// The wire name. The only sanctioned enum-to-string step, for use
            /// at the wire boundary.
            pub const fn name(self) -> &'static str {
                match self { $( Self::$tvar => $tname, )+ }
            }

            /// This topic's entry in [`METHODS`].
            pub fn descriptor(self) -> &'static MethodDescriptor {
                // Topics close METHODS, after services and actions.
                &METHODS[ServiceId::ALL.len() + ActionId::ALL.len() + self as usize]
            }
        }

        /// Every core-node wire method, in declaration order (services, then
        /// actions, then topics — the order the `descriptor()` index math and
        /// the generated AsyncAPI document depend on). `clock` appears twice
        /// (service + topic); `(name, kind)` pairs are unique, enforced by the
        /// registry tests.
        pub static METHODS: &[MethodDescriptor] = &[
            $( MethodDescriptor {
                name: $sname,
                host: Host::$shost,
                summary: $ssummary,
                payloads: Payloads::Service {
                    request: pd!($sreq, $sschema),
                    response: pd!($sresp, $sschema),
                },
            }, )+
            $( MethodDescriptor {
                name: $aname,
                host: Host::CoreNodeDaemon,
                summary: $asummary,
                payloads: Payloads::Action {
                    goal: pd!($agoal, $aschema),
                    goal_response: pd!($agoal_resp, $aschema),
                    feedback: pd!($afb, $aschema),
                    result: pd!($ares, $aschema),
                },
            }, )+
            $( MethodDescriptor {
                name: $tname,
                host: Host::CoreNodeDaemon,
                summary: $tsummary,
                payloads: Payloads::Topic {
                    message: pd!($tmsg, $tschema),
                },
            }, )+
        ];
    };
}

// `macro_rules!` items are textually scoped; these re-exports turn them into
// path-based items so the invocation site can `use machinery::{methods, pd}`.
// `service_request_impl` rides along because `methods!` expands calls to it
// at that same invocation site.
pub(crate) use launch_scoped_impl;
pub(crate) use methods;
pub(crate) use pd;
pub(crate) use service_request_impl;
