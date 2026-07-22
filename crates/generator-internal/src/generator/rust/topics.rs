use super::deserialization::build_deserialize_fn;
use super::serialization::{MessageEncodingSpec, build_serialize_payload};
use super::services::deserialize_fields_from_format;
use crate::error::Result;
use config::node::{Cardinality, ConsumedTopic, NativeEmittedTopic, QoSProfile};
use encoding::{CapnpSchemaArtifacts, FunctionParam};
use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

pub struct ConsumedTopicSubscriptionSpec<'a> {
    pub helper_fn_ident: &'a Ident,
    pub args_struct_ident: &'a Ident,
    pub params: &'a [FunctionParam],
    pub artifacts: &'a CapnpSchemaArtifacts,
    pub encoding: &'a MessageEncodingSpec,
    pub topic: &'a ConsumedTopic,
    pub struct_prefix: &'a str,
    pub dependency: &'a crate::generator::types::DependencyContext,
}

/// Specification for building an action-feedback emit-style method
/// (publish_feedback / complete).
pub struct EmitMethodSpec<'a> {
    pub method_name: &'a Ident,
    pub params: &'a [FunctionParam],
    pub encoding: Option<&'a MessageEncodingSpec>,
    /// Method receiver and leading params, e.g. `node_runner: &crate::NodeRunner` or `&self`.
    pub receiver: TokenStream,
    /// The code that publishes a serialized payload (used when encoding exists).
    pub publish_body: TokenStream,
    /// Expression for the error context in both success and error paths.
    pub error_context: TokenStream,
    /// Extra unused-suppression statements for the None (no-encoding) branch,
    /// e.g. `let _ = node_runner;` when the receiver would otherwise be unused.
    pub suppress_unused: Vec<TokenStream>,
}

/// Builds an emit-style async method that serializes params and publishes.
///
/// Shared logic for the action `publish_feedback` / `complete` methods:
/// filters instance_id from params, branches on encoding presence, and either
/// serializes + publishes or returns `MessageFormatUnavailable`.
pub fn build_emit_method(spec: EmitMethodSpec<'_>) -> TokenStream {
    let EmitMethodSpec {
        method_name,
        params,
        encoding,
        receiver,
        publish_body,
        error_context,
        suppress_unused,
    } = spec;

    let mut method_param_tokens = Vec::new();
    let instance_id_ident = Ident::new("instance_id", proc_macro2::Span::call_site());
    for param in params {
        if param.ident == instance_id_ident {
            continue;
        }
        let ident = &param.ident;
        let ty = &param.ty;
        method_param_tokens.push(quote!(#ident: #ty));
    }

    let method_signature = if method_param_tokens.is_empty() {
        receiver
    } else {
        quote!(#receiver, #(#method_param_tokens),*)
    };

    match encoding {
        Some(spec) => {
            let serialize_block =
                build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context);

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_name(#method_signature) -> crate::Result<()> {
                    let payload = #serialize_block;
                    #publish_body
                    Ok(())
                }
            }
        }
        None => {
            let ignore_params: Vec<TokenStream> = params
                .iter()
                .map(|param| {
                    let ident = &param.ident;
                    quote!(let _ = #ident;)
                })
                .collect();

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_name(#method_signature) -> crate::Result<()> {
                    #(#suppress_unused)*
                    #(#ignore_params)*
                    Err(crate::Error::MessageFormatUnavailable {
                        context: #error_context,
                    })
                }
            }
        }
    }
}

/// Generates the publish-side API for an emitted topic: a pure `build_message`
/// that serializes the message fields to a wire `peppylib::Payload`, and an
/// async `declare_publisher` that takes the central messenger lock once and
/// returns a lock-free `peppylib::TopicPublisher`. A publish loop declares the
/// publisher once and then calls `publisher.publish(build_message(...)?)`, so
/// per-message serialization never re-takes the messenger lock. This is the
/// only topic-publish path.
pub fn build_topic_publisher(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    topic: &NativeEmittedTopic,
    label: &str,
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> TokenStream {
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);
    let target_expr = sender_target_expression(origin);

    let build_message = build_topic_build_message(params, encoding, &label_literal);

    quote! {
        #build_message

        /// Declares the topic publisher: takes the central messenger lock once
        /// and returns a lock-free `peppylib::TopicPublisher`. Declare once,
        /// then call `publisher.publish(build_message(...)?)` per message.
        pub async fn declare_publisher(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<peppylib::TopicPublisher> {
            let qos = #qos_tokens;
            let as_topic = #topic_literal;
            let as_instance_id = node_runner.processor().bound_instance_id();
            let with_core_node = node_runner.processor().bound_core_node();
            let as_target = #target_expr;

            let publisher = peppylib::TopicMessenger::declare_publisher(
                node_runner.messenger(),
                with_core_node,
                as_instance_id,
                as_target,
                None,
                as_topic,
                qos,
            )
            .await?;
            Ok(publisher)
        }
    }
}

/// `build_message(fields…)` for an emitted topic: serializes the message fields
/// to a wire `peppylib::Payload` with no messenger access, so a publish loop can
/// build the payload off any lock and hand the finished bytes to
/// [`peppylib::TopicPublisher::publish`]. Filters the reserved `instance_id`
/// from the params and returns `MessageFormatUnavailable` when the topic has no
/// serializable message format.
fn build_topic_build_message(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label_literal: &Literal,
) -> TokenStream {
    let instance_id_ident = Ident::new("instance_id", proc_macro2::Span::call_site());
    let kept_params: Vec<&FunctionParam> = params
        .iter()
        .filter(|param| param.ident != instance_id_ident)
        .collect();
    let method_param_tokens: Vec<TokenStream> = kept_params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();

    let error_context = quote!(String::from(#label_literal));

    match encoding {
        Some(spec) => {
            let serialize_block =
                build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context);

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub fn build_message(#(#method_param_tokens),*) -> crate::Result<peppylib::Payload> {
                    let payload = #serialize_block;
                    Ok(payload)
                }
            }
        }
        None => {
            let ignore_params: Vec<TokenStream> = kept_params
                .iter()
                .map(|param| {
                    let ident = &param.ident;
                    quote!(let _ = #ident;)
                })
                .collect();

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub fn build_message(#(#method_param_tokens),*) -> crate::Result<peppylib::Payload> {
                    #(#ignore_params)*
                    Err(crate::Error::MessageFormatUnavailable {
                        context: #error_context,
                    })
                }
            }
        }
    }
}

/// Module-level constants + `paired()`/`wait_paired()` helpers shared by both
/// directions of a peer topic module. Every `paired_topics/<link_id>/<topic>`
/// module carries its slot identity as consts and exposes the slot's live pin
/// state.
pub fn build_peer_module_header(
    topic_name: &str,
    peer: &crate::generator::types::PeerContext,
) -> TokenStream {
    let topic_literal = Literal::string(topic_name);
    let link_id_literal = Literal::string(&peer.link_id);
    let pairing_name_literal = Literal::string(&peer.pairing_name);
    let pairing_tag_literal = Literal::string(&peer.pairing_tag);

    quote! {
        pub const TOPIC_NAME: &str = #topic_literal;
        /// This node's own pairing-slot link_id.
        pub const LINK_ID: &str = #link_id_literal;
        pub const PAIRING_NAME: &str = #pairing_name_literal;
        pub const PAIRING_TAG: &str = #pairing_tag_literal;

        /// The peer currently paired on this slot, or `None` while unpaired.
        pub fn paired(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<Option<peppylib::messaging::PeerInfo>> {
            Ok(node_runner.peer(LINK_ID)?.paired())
        }

        /// Waits until a peer is paired on this slot and returns its
        /// identity. Returns immediately when already paired.
        pub async fn wait_paired(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<peppylib::messaging::PeerInfo> {
            node_runner.peer(LINK_ID)?.wait_paired().await
        }
    }
}

/// Header for an observer topic module: same slot-identity consts as a peer
/// module, but it exposes the resolved `source()` instead of the peer helpers.
/// An observer plays no role, so there is no `paired()`/`wait_paired()`.
pub fn build_observed_module_header(
    topic_name: &str,
    observer: &crate::generator::types::PeerContext,
) -> TokenStream {
    let topic_literal = Literal::string(topic_name);
    let link_id_literal = Literal::string(&observer.link_id);
    let pairing_name_literal = Literal::string(&observer.pairing_name);
    let pairing_tag_literal = Literal::string(&observer.pairing_tag);

    quote! {
        pub const TOPIC_NAME: &str = #topic_literal;
        /// This node's own observer-slot link_id.
        pub const LINK_ID: &str = #link_id_literal;
        pub const PAIRING_NAME: &str = #pairing_name_literal;
        pub const PAIRING_TAG: &str = #pairing_tag_literal;

        /// The resolved source of this observer slot, or `None` before the
        /// daemon has delivered it. Purely local configuration state; there is
        /// no health-derived helper (a third node's health is not knowable).
        pub fn source(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<Option<peppylib::messaging::ObservedSource>> {
            Ok(node_runner.observation_slot(LINK_ID)?.source())
        }
    }
}

/// Consume side of an observer topic: a subscription backed by
/// `peppylib::runtime::subscribe_observed`, which follows the observed source
/// instance's lifecycle (fully pinned to the source triple, held across the
/// source's peer transitions, drop-before-redeclare on a source-generation
/// change). There is no publisher: an observer only reads.
pub fn build_observed_topic_subscription(spec: PeerTopicSubscriptionSpec<'_>) -> Result<TokenStream> {
    let PeerTopicSubscriptionSpec {
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        qos_profile,
        struct_prefix,
    } = spec;
    let qos_tokens = qos_profile_tokens(qos_profile);
    let helper_fn_tokens = build_topic_deserialize_helper(
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        struct_prefix,
    )?;

    let subscription_tokens = build_subscription_struct(
        quote! {
            /// A held subscription to an observed pairing topic. Yields nothing
            /// while the source is unresolved or not emitting; while the source
            /// is live, only its messages surface (triple wire pin +
            /// generation-tagged delivery check). A pairing is a live stream,
            /// not a mailbox: messages published before observation are never
            /// delivered.
        },
        quote!(peppylib::runtime::ObservedTopicSubscription),
        quote! {
            /// Awaits the next message from the currently observed source.
            ///
            /// Returns `Ok(Some((producer, message)))` for each message,
            /// `Ok(None)` once the runtime shuts down, and `Err(..)` if a
            /// received payload fails to deserialize. `producer` is always the
            /// observed source instance.
        },
        quote! {
            let Some((producer, message)) = self.inner.next().await else {
                return Ok(None);
            };
        },
        helper_fn_ident,
        args_struct_ident,
    );

    Ok(quote! {
        #subscription_tokens

        /// Subscribes to this observed pairing topic and returns a held
        /// `Subscription` pinned to the observer slot's source. Legal before
        /// the source is resolved or live: the subscription stays silent until
        /// the source emits.
        pub async fn subscribe(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<Subscription> {
            let qos = #qos_tokens;
            let inner = peppylib::runtime::subscribe_observed(
                node_runner,
                LINK_ID,
                PAIRING_NAME,
                PAIRING_TAG,
                TOPIC_NAME,
                qos,
            )
            .await?;
            Ok(Subscription { inner })
        }

        #helper_fn_tokens
    })
}

/// Publish side of a peer-emitted topic: `build_message` (same shape as
/// emitted topics) plus a slot-scoped `declare_publisher` — the wire target
/// is `SenderTarget::pairing(...)` and the producer-side link_id segment
/// carries this node's OWN slot link_id, so per-slot streams stay
/// wire-isolated (the slot IS the identity; no payload demux).
pub fn build_peer_topic_publisher(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    qos_profile: &QoSProfile,
    label: &str,
) -> TokenStream {
    let qos_tokens = qos_profile_tokens(qos_profile);
    let label_literal = Literal::string(label);
    let build_message = build_topic_build_message(params, encoding, &label_literal);

    quote! {
        #build_message

        /// Declares the slot-scoped publisher for this pairing topic.
        /// Publishing while unpaired is a legal no-op (the mesh drops it);
        /// the paired peer's triple-pinned subscription receives every
        /// publish made while the pair is live.
        pub async fn declare_publisher(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<peppylib::TopicPublisher> {
            let qos = #qos_tokens;
            let as_instance_id = node_runner.processor().bound_instance_id();
            let with_core_node = node_runner.processor().bound_core_node();
            let as_target = peppylib::messaging::SenderTarget::pairing(PAIRING_NAME, PAIRING_TAG)?;

            let publisher = peppylib::TopicMessenger::declare_publisher(
                node_runner.messenger(),
                with_core_node,
                as_instance_id,
                as_target,
                Some(LINK_ID),
                TOPIC_NAME,
                qos,
            )
            .await?;
            Ok(publisher)
        }
    }
}

/// Consume side of a peer topic: a subscription backed by
/// `peppylib::runtime::subscribe_peer`, which follows the slot's live pin
/// (silent while unpaired, triple wire pin while paired, drop-before-redeclare
/// on re-pin). The binding-slot consumer filters are never involved.
pub struct PeerTopicSubscriptionSpec<'a> {
    pub helper_fn_ident: &'a Ident,
    pub args_struct_ident: &'a Ident,
    pub params: &'a [FunctionParam],
    pub artifacts: &'a CapnpSchemaArtifacts,
    pub encoding: &'a MessageEncodingSpec,
    pub qos_profile: &'a QoSProfile,
    pub struct_prefix: &'a str,
}

pub fn build_peer_topic_subscription(spec: PeerTopicSubscriptionSpec<'_>) -> Result<TokenStream> {
    let PeerTopicSubscriptionSpec {
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        qos_profile,
        struct_prefix,
    } = spec;
    let qos_tokens = qos_profile_tokens(qos_profile);
    let helper_fn_tokens = build_topic_deserialize_helper(
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        struct_prefix,
    )?;

    let subscription_tokens = build_subscription_struct(
        quote! {
            /// A held subscription to this pairing topic. Yields nothing while
            /// the slot is unpaired; while paired, only the paired peer's
            /// messages surface (triple wire pin + delivery-time identity
            /// check). A pairing is a live stream, not a mailbox: messages
            /// published before pairing are never delivered.
        },
        quote!(peppylib::runtime::PeerSubscription),
        quote! {
            /// Awaits the next message from the currently paired peer.
            ///
            /// Returns `Ok(Some((producer, message)))` for each message,
            /// `Ok(None)` once the runtime shuts down, and `Err(..)` if a
            /// received payload fails to deserialize. `producer` is always
            /// the paired peer's identity.
        },
        quote! {
            let Some(message) = self.inner.on_next_message().await else {
                return Ok(None);
            };
            let producer = peppylib::messaging::ProducerRef::new(
                message.core_node(),
                message.instance_id(),
            );
        },
        helper_fn_ident,
        args_struct_ident,
    );

    Ok(quote! {
        #subscription_tokens

        /// Subscribes to this pairing topic and returns a held
        /// `Subscription` that follows the slot's live pin. Legal while
        /// unpaired: the subscription stays silent until a peer pairs.
        pub async fn subscribe(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<Subscription> {
            let qos = #qos_tokens;
            let inner = peppylib::runtime::subscribe_peer(
                node_runner,
                LINK_ID,
                PAIRING_NAME,
                PAIRING_TAG,
                TOPIC_NAME,
                qos,
            )
            .await?;
            Ok(Subscription { inner })
        }

        #helper_fn_tokens
    })
}

/// The held-`Subscription` struct plus its `next()` impl, shared by the
/// bound-set and peer consumer modules: same decode contract, differing in
/// the inner subscription type, how `(producer, message)` is received from
/// it (`recv_tokens`, statements that bind `producer` and `message` or
/// early-return `Ok(None)`), and the doc text (passed as `quote!`d `///`
/// blocks so the generated docs stay per-module).
fn build_subscription_struct(
    struct_doc: TokenStream,
    inner_type: TokenStream,
    next_doc: TokenStream,
    recv_tokens: TokenStream,
    helper_fn_ident: &Ident,
    args_struct_ident: &Ident,
) -> TokenStream {
    quote! {
        #struct_doc
        pub struct Subscription {
            inner: #inner_type,
        }

        impl Subscription {
            #next_doc
            // An async `next` returning `Result<Option<_>>` is intentionally not
            // `Iterator::next`; silence clippy's lookalike heuristic.
            #[allow(clippy::should_implement_trait)]
            pub async fn next(
                &mut self,
            ) -> crate::Result<Option<(peppylib::messaging::ProducerRef, #args_struct_ident)>> {
                #recv_tokens

                let payload = message.payload();
                let message = #helper_fn_ident(payload.as_ref())?;
                Ok(Some((producer, message)))
            }
        }
    }
}

/// Returns the `SenderTarget` constructor expression to splice into a
/// generated emit call. When `origin` is `Some` (the topic is declared via
/// a `manifest.implements` slot), emit as `SenderTarget::contract(name, tag)`.
/// Otherwise emit as `SenderTarget::node(node_name, node_tag)` using the
/// runtime's own identity. Both forms are fallible because segment validation
/// runs at construction.
pub fn sender_target_expression(
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> TokenStream {
    match origin {
        Some(o) => {
            let name = Literal::string(&o.contract_name);
            let tag = Literal::string(&o.contract_tag);
            quote!(peppylib::messaging::SenderTarget::contract(#name, #tag)?)
        }
        None => quote!(peppylib::messaging::SenderTarget::node(
            node_runner.processor().node_name(),
            node_runner.processor().node_tag(),
        )?),
    }
}

pub fn build_consumed_topic_subscription(
    spec: ConsumedTopicSubscriptionSpec,
) -> Result<TokenStream> {
    let ConsumedTopicSubscriptionSpec {
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        topic,
        struct_prefix,
        dependency,
    } = spec;
    let topic_literal = Literal::string(&topic.name);
    let node_name_literal = Literal::string(&dependency.producer_name);
    let helper_fn_tokens = build_topic_deserialize_helper(
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        struct_prefix,
    )?;

    let from_target_expr = consumed_to_target_expression(dependency);
    let bound_producers_expr = consumed_bound_producers_expression(dependency);
    let bound_producers_fn = build_bound_producers_fn(dependency);

    let subscription_tokens = build_subscription_struct(
        quote! {
            /// A held subscription covering every producer bound to this slot: one
            /// producer-pinned wire subscription per member of the slot's bound
            /// set, merged client-side. Message order is preserved independently
            /// per producer (no total ordering across producers), ready producers
            /// are merged fairly, and the bound set is fixed at startup. To follow
            /// a single producer, filter on the yielded producer identity.
        },
        quote!(peppylib::messaging::BoundSetSubscription),
        quote! {
            /// Awaits the next message from any producer bound to this slot.
            ///
            /// Returns `Ok(Some((producer, message)))` for each message (the
            /// producer that published it always rides along), `Ok(None)` once the
            /// node is shutting down and no queued message remains, and `Err(..)`
            /// if a received payload fails to deserialize; a decode error does not
            /// tear down the subscription or shrink the bound set. On an empty
            /// `zero_or_more` set this pends until shutdown, then returns
            /// `Ok(None)`.
        },
        quote! {
            let Some((producer, message)) = self.inner.on_next_message().await else {
                return Ok(None);
            };
        },
        helper_fn_ident,
        args_struct_ident,
    );

    Ok(quote! {
        #bound_producers_fn

        #subscription_tokens

        /// Subscribes to this topic across the slot's complete bound producer
        /// set and returns a held `Subscription` yielding `(producer, message)`.
        ///
        /// The shape is identical for every cardinality; only the size of the
        /// bound set changes. Call this once, then loop on `Subscription::next`;
        /// each underlying subscription's buffer retains messages published
        /// between calls, so nothing is lost to a re-subscribe gap.
        pub async fn subscribe(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<Subscription> {
            let topic_name = #topic_literal;
            let node_name = #node_name_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let inner = peppylib::TopicMessenger::subscribe_bound_set(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #from_target_expr,
                topic_name,
                #bound_producers_expr,
                qos,
                node_runner.cancellation_token().clone(),
            )
            .await
            .map_err(|source| crate::Error::TopicSubscribe {
                topic_name: topic_name.to_string(),
                node_name: node_name.to_string(),
                source_msg: source.to_string(),
            })?;

            Ok(Subscription { inner })
        }

        #helper_fn_tokens
    })
}

/// Returns the `&[ProducerRef]` expression spliced into generated
/// [`peppylib::TopicMessenger::subscribe_bound_set`] calls (and the public
/// `bound_producers()` body of `zero_or_more` slots):
/// `Processor::bound_producers(<manifest link_id>)`, the slot's
/// runtime-resolved ordered producer set. The validator pre-resolves the
/// consumer's launcher / CLI binding map (sized per the slot's cardinality;
/// anything else is rejected at launch) and the runtime processor caches
/// the sets at startup, so the lookup is an infallible borrow shared by
/// every interface kind. `subscribe()` always takes the plain slice: the
/// merged wire subscription covers the complete set whatever the
/// cardinality, so only the public accessor is cardinality-typed.
pub fn consumed_bound_producers_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    let literal = Literal::string(&dependency.link_id);
    quote!(node_runner.processor().bound_producers(#literal))
}

/// The module-level bound-producer accessor every consumed topic, service,
/// and action module exposes. Its name and return type encode the slot's
/// launch-validated cardinality instead of restating it in comments: `one`
/// generates `bound_producer()` returning the sole producer directly,
/// `one_or_more` generates `bound_producers()` returning a never-empty
/// `NonEmptyProducers` whose `first()` is infallible, and `zero_or_more`
/// generates `bound_producers()` returning a plain, possibly empty slice,
/// so the empty branch is forced by the type and is never dead code. Every
/// module sharing this slot's `link_id` returns the same set in the same
/// application declaration order, and flipping a slot's cardinality
/// surfaces every call site that relied on the old guarantee at compile
/// time. The accessor doc comes from
/// [`DependencyContext::bound_producers_doc`] so both language generators
/// state the same guarantees; only the Rust-API tail sentence is added
/// here.
///
/// [`DependencyContext::bound_producers_doc`]: crate::generator::types::DependencyContext::bound_producers_doc
pub fn build_bound_producers_fn(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    let link_id_literal = Literal::string(&dependency.link_id);
    let (api_note, accessor) = match dependency.cardinality {
        Cardinality::One => (
            None,
            quote! {
                pub fn bound_producer(
                    node_runner: &crate::NodeRunner,
                ) -> &peppylib::messaging::ProducerRef {
                    node_runner.processor().sole_bound_producer(#link_id_literal)
                }
            },
        ),
        Cardinality::OneOrMore => (
            Some("`first()` is infallible."),
            quote! {
                pub fn bound_producers(
                    node_runner: &crate::NodeRunner,
                ) -> peppylib::messaging::NonEmptyProducers<'_> {
                    node_runner.processor().non_empty_bound_producers(#link_id_literal)
                }
            },
        ),
        Cardinality::ZeroOrMore => (
            None,
            quote! {
                pub fn bound_producers(
                    node_runner: &crate::NodeRunner,
                ) -> &[peppylib::messaging::ProducerRef] {
                    node_runner.processor().bound_producers(#link_id_literal)
                }
            },
        ),
    };
    let mut doc_lines: Vec<&str> = dependency.bound_producers_doc().to_vec();
    if let Some(note) = api_note {
        doc_lines.push(note);
    }
    let doc = super::doc_attrs(&doc_lines);
    quote! {
        #(#doc)*
        #accessor
    }
}

/// Returns the `SenderTarget` expression spliced into a generated
/// `TopicMessenger::subscribe` / `ServiceMessenger::poll` /
/// `ActionMessenger::send_goal` call. When the dependency emits via
/// a contract, the consumer matches on `Contract(name, tag)`; otherwise
/// on `Node(node_name, node_tag)`.
pub fn consumed_to_target_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    match &dependency.origin {
        Some(origin) => {
            let name = Literal::string(&origin.contract_name);
            let tag = Literal::string(&origin.contract_tag);
            quote!(peppylib::messaging::SenderTarget::contract(#name, #tag)?)
        }
        None => {
            let node_name = Literal::string(&dependency.producer_name);
            let node_tag = Literal::string(&dependency.producer_tag);
            quote!(peppylib::messaging::SenderTarget::node(#node_name, #node_tag)?)
        }
    }
}

fn build_topic_deserialize_helper(
    helper_fn_ident: &Ident,
    args_struct_ident: &Ident,
    params: &[FunctionParam],
    artifacts: &CapnpSchemaArtifacts,
    encoding: &MessageEncodingSpec,
    struct_prefix: &str,
) -> Result<TokenStream> {
    let reader_type = &encoding.reader_type;
    let context_literal = Literal::string(struct_prefix);
    let context_expr = quote!(String::from(#context_literal));

    let (field_statements, value_idents) = deserialize_fields_from_format(
        artifacts.message_format(),
        params,
        struct_prefix,
        &context_expr,
    )?;
    let field_inits: Vec<TokenStream> = params
        .iter()
        .zip(value_idents.iter())
        .map(|(param, value_ident)| {
            let field_ident = &param.ident;
            quote!(#field_ident: #value_ident)
        })
        .collect();

    Ok(build_deserialize_fn(
        helper_fn_ident,
        reader_type,
        &context_expr,
        &quote!(#args_struct_ident),
        &field_statements,
        &quote!(#args_struct_ident { #( #field_inits ),* }),
    ))
}

pub fn qos_profile_tokens(profile: &QoSProfile) -> TokenStream {
    let variant = match profile {
        QoSProfile::Standard => "Standard",
        QoSProfile::Reliable => "Reliable",
        QoSProfile::SensorData => "SensorData",
        QoSProfile::Critical => "Critical",
    };
    let variant_ident = Ident::new(variant, proc_macro2::Span::call_site());
    quote!(peppylib::config::QoSProfile::#variant_ident)
}
