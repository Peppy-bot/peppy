use super::context::SchemaFieldLookup;
use super::deserialization::generate_field_reader_statements;
use super::serialization::{MessageEncodingSpec, NameGenerator};
use config::encoding::{CapnpSchemaArtifacts, FunctionParam};
use config::node::{ExposedTopic, QoSProfile, SubscribedTopic};
use proc_macro2::{Ident, Literal, TokenStream};
use quote::quote;

pub(super) fn build_topic_emit(
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
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let init_root_tokens = if assignments.is_empty() {
                quote!(let _ = capnp_msg.init_root::<#builder_type>();)
            } else {
                quote! {
                    let mut root = capnp_msg.init_root::<#builder_type>();
                    #(#assignments)*
                }
            };

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_ident(#method_signature) -> crate::Result<()> {
                    let mut capnp_msg = capnp::message::Builder::new_default();
                    {
                        #init_root_tokens
                    }

                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &capnp_msg).map_err(|source| {
                        crate::Error::CapnpSerialize {
                            context: String::from(#label_literal),
                            source,
                        }
                    })?;

                    let payload = bytes::Bytes::from(buffer);
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

#[allow(clippy::too_many_arguments)]
pub(super) fn build_subscribed_topic_callback(
    fn_name: &Ident,
    helper_fn_ident: &Ident,
    args_struct_ident: &Ident,
    params: &[FunctionParam],
    artifacts: &CapnpSchemaArtifacts,
    encoding: &MessageEncodingSpec,
    topic: &SubscribedTopic,
    struct_prefix: &str,
) -> TokenStream {
    let topic_literal = Literal::string(topic.name.as_str());
    let node_name_literal = Literal::string(topic.node.as_str());
    let reader_type = &encoding.reader_type;
    let schema_lookup = SchemaFieldLookup::new(artifacts.message_format());

    let context_literal = Literal::string(struct_prefix);
    let context_expr = quote!(String::from(#context_literal));

    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();
    for param in params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&key);
        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            original_name.as_str(),
            schema,
            struct_prefix,
            &context_expr,
            &mut names,
        );
        field_statements.append(&mut statements);
        let field_ident = &param.ident;
        field_inits.push(quote!(#field_ident: #value_ident));
    }

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

        fn #helper_fn_ident(payload: &[u8]) -> crate::Result<#args_struct_ident> {
            let mut cursor = std::io::Cursor::new(payload);
            let reader_options = capnp::message::ReaderOptions::new();
            let message_reader = capnp::serialize::read_message(&mut cursor, reader_options)
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: String::from(#context_literal),
                    source,
                })?;

            let root = message_reader
                .get_root::<#reader_type>()
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: String::from(#context_literal),
                    source,
                })?;

            #(#field_statements)*

            Ok(#args_struct_ident {
                #( #field_inits ),*
            })
        }
    }
}

pub(super) fn qos_profile_tokens(profile: &QoSProfile) -> TokenStream {
    let variant = match profile {
        QoSProfile::Standard => "Standard",
        QoSProfile::Reliable => "Reliable",
        QoSProfile::SensorData => "SensorData",
        QoSProfile::Critical => "Critical",
    };
    let variant_ident = Ident::new(variant, proc_macro2::Span::call_site());
    quote!(peppylib::config::QoSProfile::#variant_ident)
}
