use super::deserialization::build_deserialize_fn;
use super::serialization::{MessageEncodingSpec, build_serialize_payload};
use super::services::deserialize_fields_from_format;
use config::encoding::{CapnpSchemaArtifacts, FunctionParam};
use config::node::{ExposedTopic, QoSProfile, SubscribedTopic};
use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

pub struct SubscribedTopicCallbackSpec<'a> {
    pub fn_name: &'a Ident,
    pub helper_fn_ident: &'a Ident,
    pub args_struct_ident: &'a Ident,
    pub params: &'a [FunctionParam],
    pub artifacts: &'a CapnpSchemaArtifacts,
    pub encoding: &'a MessageEncodingSpec,
    pub topic: &'a SubscribedTopic,
    pub struct_prefix: &'a str,
}

pub fn build_topic_emit(
    method_ident: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    topic: &ExposedTopic,
    label: &str,
) -> TokenStream {
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
        quote!(node_runner: &crate::NodeRunner)
    } else {
        quote!(node_runner: &crate::NodeRunner, #(#method_param_tokens),*)
    };
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);

    match encoding {
        Some(spec) => {
            let error_context = quote!(String::from(#label_literal));
            let serialize_block =
                build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context);

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_ident(#method_signature) -> crate::Result<()> {
                    let payload = #serialize_block;
                    let qos = #qos_tokens;
                    let as_topic = #topic_literal;
                    let as_node_name = node_runner.processor().node_name();
                    let as_instance_id = node_runner.processor().bound_instance_id();
                    let with_master_node = node_runner.processor().bound_master_node();

                    peppylib::TopicMessenger::emit(
                        node_runner.messenger(),
                        with_master_node,
                        as_instance_id,
                        as_node_name,
                        as_topic,
                        qos,
                        payload,
                    )
                        .await?;
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
                pub async fn #method_ident(#method_signature) -> crate::Result<()> {
                    let _ = node_runner;
                    #(#ignore_params)*
                    Err(crate::Error::MessageFormatUnavailable {
                        context: String::from(#label_literal),
                    })
                }
            }
        }
    }
}

pub fn build_subscribed_topic_callback(spec: SubscribedTopicCallbackSpec) -> TokenStream {
    let SubscribedTopicCallbackSpec {
        fn_name,
        helper_fn_ident,
        args_struct_ident,
        params,
        artifacts,
        encoding,
        topic,
        struct_prefix,
    } = spec;
    let topic_literal = Literal::string(topic.name.as_str());
    let node_name_literal = Literal::string(topic.node.as_str());
    let reader_type = &encoding.reader_type;
    let context_literal = Literal::string(struct_prefix);
    let context_expr = quote!(String::from(#context_literal));

    let (field_statements, value_idents) = deserialize_fields_from_format(
        artifacts.message_format(),
        params,
        struct_prefix,
        &context_expr,
    );
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

    quote! {
        pub async fn #fn_name(
            node_runner: &crate::NodeRunner,
            master_node_target: Option<&str>,
            instance_id_target: Option<&str>,
        ) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let node_name = #node_name_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let subscription_future = peppylib::TopicMessenger::subscribe(
                    node_runner.messenger(),
                    node_runner.processor().bound_master_node(),
                    node_runner.processor().bound_instance_id(),
                    node_name,
                    topic_name,
                    master_node_target,
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

            let payload = message.payload().as_bytes();
            let instance_id = message.instance_id().to_string();
            let message = #helper_fn_ident(payload.as_ref())?;
            Ok((instance_id, message))
        }

        #helper_fn_tokens
    }
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
