use super::deserialization::build_deserialize_fn;
use super::serialization::{MessageEncodingSpec, build_serialize_payload};
use super::services::deserialize_fields_from_format;
use crate::error::Result;
use config::encoding::{CapnpSchemaArtifacts, FunctionParam};
use config::node::{ConsumedTopic, EmittedTopic, QoSProfile};
use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

pub struct ConsumedTopicCallbackSpec<'a> {
    pub fn_name: &'a Ident,
    pub helper_fn_ident: &'a Ident,
    pub args_struct_ident: &'a Ident,
    pub params: &'a [FunctionParam],
    pub artifacts: &'a CapnpSchemaArtifacts,
    pub encoding: &'a MessageEncodingSpec,
    pub topic: &'a ConsumedTopic,
    pub struct_prefix: &'a str,
    pub dependency: &'a crate::generator::types::DependencyContext,
}

/// Specification for building an emit-style method (topic emit or action feedback emit).
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
/// Shared logic for both `build_topic_emit` and `build_action_feedback_emit`:
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

pub fn build_topic_emit(
    method_ident: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    topic: &EmittedTopic,
    label: &str,
    origin: Option<&crate::generator::types::InterfaceOrigin>,
) -> TokenStream {
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);
    let target_expr = sender_target_expression(origin);

    build_emit_method(EmitMethodSpec {
        method_name: method_ident,
        params,
        encoding,
        receiver: quote!(node_runner: &crate::NodeRunner),
        publish_body: quote! {
            let qos = #qos_tokens;
            let as_topic = #topic_literal;
            let as_instance_id = node_runner.processor().bound_instance_id();
            let with_core_node = node_runner.processor().bound_core_node();
            let as_target = #target_expr;

            peppylib::TopicMessenger::emit(
                node_runner.messenger(),
                with_core_node,
                as_instance_id,
                as_target,
                as_topic,
                qos,
                payload,
            )
                .await?;
        },
        error_context: quote!(String::from(#label_literal)),
        suppress_unused: vec![quote!(let _ = node_runner;)],
    })
}

/// Returns the `SenderTarget` constructor expression to splice into a
/// generated emit call. When `origin` is `Some` (the topic is declared via
/// `interfaces.conforms_to`), emit as `SenderTarget::interface(name, tag)`.
/// Otherwise emit as `SenderTarget::node(node_name, node_tag)` using the
/// runtime's own identity. Both forms are fallible because segment validation
/// runs at construction.
pub fn sender_target_expression(
    origin: Option<&crate::generator::types::InterfaceOrigin>,
) -> TokenStream {
    match origin {
        Some(o) => {
            let name = Literal::string(&o.iface_name);
            let tag = Literal::string(&o.iface_tag);
            quote!(peppylib::messaging::SenderTarget::interface(#name, #tag)?)
        }
        None => quote!(peppylib::messaging::SenderTarget::node(
            node_runner.processor().node_name(),
            node_runner.processor().node_tag(),
        )?),
    }
}

pub fn build_consumed_topic_callback(spec: ConsumedTopicCallbackSpec) -> Result<TokenStream> {
    let ConsumedTopicCallbackSpec {
        fn_name,
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

    let from_target_expr = consumed_from_target_expression(dependency);
    let consumer_filter_expr = consumed_consumer_filter_expression(dependency);
    let is_from_any_lit = is_from_any_literal(dependency);

    Ok(quote! {
        pub async fn #fn_name(
            node_runner: &crate::NodeRunner,
        ) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let node_name = #node_name_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let subscription_future = peppylib::TopicMessenger::subscribe(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    #from_target_expr,
                    #is_from_any_lit,
                    topic_name,
                    #consumer_filter_expr,
                    qos,
                );
                let mut subscription = subscription_future.await.map_err(|source| {
                    crate::Error::TopicSubscribe {
                        topic_name: topic_name.to_string(),
                        node_name: node_name.to_string(),
                        source_msg: source.to_string(),
                    }
                })?;
                subscription
                    .on_next_message()
                    .await
                    .ok_or_else(|| crate::Error::SubscriptionClosed {
                        topic_name: topic_name.to_string(),
                    })?
            };

            let payload = message.payload();
            let instance_id = message.instance_id().to_string();
            let message = #helper_fn_ident(payload.as_ref())?;
            Ok((instance_id, message))
        }

        #helper_fn_tokens
    })
}

/// Returns the `Option<SenderTarget>` expression spliced into a generated
/// `TopicMessenger::subscribe` call to pin the consumer on a specific producer.
/// When the dependency emits via `conforms_to`, the consumer matches on
/// `Interface(name, tag)`; otherwise on `Node(node_name, node_tag)`.
pub fn consumed_from_target_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    let target = consumed_to_target_expression(dependency);
    quote!(Some(#target))
}

/// Returns the `&ConsumerFilter` expression spliced into a generated
/// [`peppylib::TopicMessenger::subscribe`] call at the consumer-filter slot.
/// Calls `Processor::consumer_filter(<manifest link_id>)` when the
/// dependency carries a `link_id` (pinned or `from_any: true`); falls
/// back to a `&ConsumerFilter::Any` reference for synthetic test
/// fixtures that don't model a manifest dep. The validator pre-resolves
/// the consumer's launcher / CLI binding map into per-slot
/// [`config::runtime::SlotBinding`] entries, and the runtime processor
/// expands those into [`peppylib::messaging::ConsumerFilter`]s — see the
/// resolver in `peppylib::messaging::filter`.
pub fn consumed_consumer_filter_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    match dependency.wire_link_id() {
        Some(link_id) => {
            let literal = Literal::string(link_id);
            quote!(node_runner.processor().consumer_filter(#literal))
        }
        None => quote!(&peppylib::messaging::ConsumerFilter::Any),
    }
}

/// Returns the `Option<&ProducerRef>` expression spliced into a generated
/// [`peppylib::ServiceMessenger::poll`] /
/// [`peppylib::ActionMessenger::send_goal`] call at the single `target`
/// slot. When `dependency.wire_link_id()` is `Some(link_id)` — i.e., any
/// real manifest dep, whether pinned or `from_any` — the emitted
/// expression calls `consumer_filter(link_id).pinned_target()`: a pinned
/// slot (or a `from_any` slot bound to a single producer) resolves at
/// runtime to that producer's full `(core_node, instance_id)` and the
/// call addresses it directly with no discovery; other variants
/// (multi-pin, wildcards) give `None` and the call site falls back to
/// wildcard discovery. Synthetic test fixtures with no manifest dep skip
/// the lookup and emit a typed `None` directly.
pub fn consumed_target_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    match dependency.wire_link_id() {
        Some(link_id) => {
            let literal = Literal::string(link_id);
            quote!(node_runner.processor().consumer_filter(#literal).pinned_target())
        }
        None => quote!(Option::<&peppylib::messaging::ProducerRef>::None),
    }
}

/// `bool` literal for the `is_from_any` argument of
/// [`peppylib::TopicMessenger::subscribe`]. `true` for `from_any: true`
/// deps, `false` for pinned deps (and the test-fixture wildcard
/// variant, which has no manifest dep and so should not reserve a
/// `from_any` slot).
pub fn is_from_any_literal(dependency: &crate::generator::types::DependencyContext) -> TokenStream {
    let is_from_any = matches!(
        dependency.link_id,
        crate::generator::types::WireLinkId::Wildcard { link_id: Some(_) }
    );
    if is_from_any {
        quote!(true)
    } else {
        quote!(false)
    }
}

/// Returns the `SenderTarget` expression spliced into a generated
/// `ServiceMessenger::poll` / `ActionMessenger::send_goal` call. Same producer
/// matching rules as [`consumed_from_target_expression`] but without the
/// `Option` wrapper since these APIs require a target.
pub fn consumed_to_target_expression(
    dependency: &crate::generator::types::DependencyContext,
) -> TokenStream {
    match &dependency.origin {
        Some(origin) => {
            let name = Literal::string(&origin.iface_name);
            let tag = Literal::string(&origin.iface_tag);
            quote!(peppylib::messaging::SenderTarget::interface(#name, #tag)?)
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
