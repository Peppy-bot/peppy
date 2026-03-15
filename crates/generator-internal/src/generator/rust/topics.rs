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
    pub dependency_node_name: &'a str,
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
) -> TokenStream {
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);

    build_emit_method(EmitMethodSpec {
        method_name: method_ident,
        params,
        encoding,
        receiver: quote!(node_runner: &crate::NodeRunner),
        publish_body: quote! {
            let qos = #qos_tokens;
            let as_topic = #topic_literal;
            let as_node_name = node_runner.processor().node_name();
            let as_instance_id = node_runner.processor().bound_instance_id();
            let with_core_node = node_runner.processor().bound_core_node();

            peppylib::TopicMessenger::emit(
                node_runner.messenger(),
                with_core_node,
                as_instance_id,
                as_node_name,
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
        dependency_node_name,
    } = spec;
    let topic_literal = Literal::string(topic.name());
    let node_name_literal = Literal::string(dependency_node_name);
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

    let helper_fn_tokens = build_deserialize_fn(
        helper_fn_ident,
        reader_type,
        &context_expr,
        &quote!(#args_struct_ident),
        &field_statements,
        &quote!(#args_struct_ident { #( #field_inits ),* }),
    );

    Ok(quote! {
        pub async fn #fn_name(
            node_runner: &crate::NodeRunner,
            core_node_target: Option<&str>,
            instance_id_target: Option<&str>,
        ) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let node_name = #node_name_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let subscription_future = peppylib::TopicMessenger::subscribe(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    node_name,
                    topic_name,
                    core_node_target,
                    instance_id_target,
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

pub struct ExternalConsumedTopicCallbackSpec<'a> {
    pub fn_name: &'a Ident,
    pub helper_fn_ident: &'a Ident,
    pub args_struct_ident: &'a Ident,
    pub params: &'a [FunctionParam],
    pub artifacts: &'a CapnpSchemaArtifacts,
    pub encoding: &'a MessageEncodingSpec,
    pub topic_name: &'a str,
    pub struct_prefix: &'a str,
}

pub fn build_external_consumed_topic_callback(
    spec: ExternalConsumedTopicCallbackSpec,
) -> Result<TokenStream> {
    let ExternalConsumedTopicCallbackSpec {
        fn_name,
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        topic_name,
        struct_prefix,
    } = spec;
    let topic_literal = Literal::string(topic_name);
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

    let helper_fn_tokens = build_deserialize_fn(
        helper_fn_ident,
        reader_type,
        &context_expr,
        &quote!(#args_struct_ident),
        &field_statements,
        &quote!(#args_struct_ident { #( #field_inits ),* }),
    );

    Ok(quote! {
        pub async fn #fn_name(
            node_runner: &crate::NodeRunner,
            core_node_target: Option<&str>,
            instance_id_target: Option<&str>,
        ) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let subscription_future = peppylib::TopicMessenger::subscribe_external(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    topic_name,
                    core_node_target,
                    instance_id_target,
                    qos,
                );
                let mut subscription = subscription_future.await.map_err(|source| {
                    crate::Error::TopicSubscribe {
                        topic_name: topic_name.to_string(),
                        node_name: String::new(),
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
