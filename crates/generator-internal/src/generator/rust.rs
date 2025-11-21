#[cfg(test)]
mod tests;

use super::checker;
use super::common;
use super::types::{
    CapnpSchema, InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage,
};
use crate::error::Error;
use crate::error::Result;
use config::encoding::{CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper};
use config::{
    consts::PEPPY_NODE_CONFIG_FILE,
    node::{
        ArraySchema, ExposedAction, ExposedService, ExposedTopic, MessageFormat, PrimitiveSchema,
        QoSProfile, SchemaType, SubscribedAction, SubscribedService, SubscribedTopic, TypeToken,
    },
};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;
use std::path::Path;
use syn::{File, parse2};

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
    pending_exposed_services: Option<ExposedServicesModule>,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
            schemas: HashMap::new(),
            pending_exposed_services: None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(mut self) -> Vec<InterfaceArtifact> {
        self.flush_pending_exposed_services();
        self.sections
    }

    fn flush_pending_exposed_services(&mut self) {
        if let Some(module) = self.pending_exposed_services.take() {
            for artifact in module.into_artifacts() {
                self.push_section(artifact);
            }
        }
    }

    fn register_schema(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: &CapnpSchemaArtifacts,
    ) -> Result<SchemaInfo> {
        let schema_source = artifacts.encoding_schema();

        let key_component = sanitize_component(schema_key);
        let base_name = if key_component.is_empty() {
            "message".to_string()
        } else {
            key_component
        };

        let file_stem = if base_name.ends_with("_message") {
            base_name
        } else {
            format!("{base_name}_message")
        };

        let struct_name = format!("{struct_prefix}Message");
        let struct_module = normalize_snake_case(&struct_name);
        let schema = schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let capnp_schema = CapnpSchema::new(file_stem.clone(), schema);
        self.schemas.insert(file_stem.clone(), capnp_schema);

        Ok(SchemaInfo {
            file_stem,
            struct_module,
        })
    }

    fn prepare_message_encoding(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: Option<&CapnpSchemaArtifacts>,
        params: &[FunctionParam],
    ) -> Result<Option<MessageEncodingSpec>> {
        let Some(artifacts) = artifacts else {
            return Ok(None);
        };

        let schema_info = self.register_schema(schema_key, struct_prefix, artifacts)?;
        let builder_ident = Ident::new("root", Span::call_site());
        let assignments =
            generate_assignments_for_format(&builder_ident, artifacts.message_format(), params);

        Ok(Some(MessageEncodingSpec {
            builder_type: schema_info.builder_type_tokens(),
            assignments,
            reader_type: schema_info.reader_type_tokens(),
        }))
    }

    fn build_action_service_handler(
        &mut self,
        fn_name: &Ident,
        handler_fn_name_override: Option<&Ident>,
        handler_helper_name_override: Option<&Ident>,
        rust_struct_prefix: &str,
        schema_struct_prefix: &str,
        request_format: Option<&MessageFormat>,
        response_format: Option<&MessageFormat>,
        label: &str,
        service_name_literal: &Literal,
        context: &mut GenerationContext,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let accept_format_artifacts = map_message_format(request_format)?;
        let return_format_artifacts = map_message_format(response_format)?;

        let params = collect_function_params(
            accept_format_artifacts.as_ref(),
            return_format_artifacts.as_ref(),
            rust_struct_prefix,
            context,
            None,
        )?;

        let encoding = self.prepare_message_encoding(
            label,
            schema_struct_prefix,
            accept_format_artifacts.as_ref(),
            &params,
        )?;

        let request_struct_ident =
            if let Some((ident, tokens)) = build_request_struct(rust_struct_prefix, &params) {
                context.add_private_struct(tokens);
                Some(ident)
            } else {
                None
            };

        let response_spec = if let Some(return_artifacts) = return_format_artifacts.as_ref() {
            let response_struct_prefix = format!("{rust_struct_prefix}Response");
            let response_schema_prefix = format!("{schema_struct_prefix}Response");
            let schema_key = format!("{label}_response");
            let schema_info =
                self.register_schema(&schema_key, &response_schema_prefix, return_artifacts)?;
            Some(ServiceResponseSpec {
                format: return_artifacts.message_format(),
                struct_ident: Ident::new(&response_struct_prefix, Span::call_site()),
                builder_type: schema_info.builder_type_tokens(),
                include_service_instance_id: false,
            })
        } else {
            None
        };

        Ok(build_exposed_service_method(
            fn_name,
            handler_fn_name_override,
            handler_helper_name_override,
            None,
            &params,
            &params,
            None,
            encoding.as_ref(),
            accept_format_artifacts
                .as_ref()
                .map(|art| art.message_format()),
            label,
            service_name_literal,
            request_struct_ident.as_ref(),
            response_spec.as_ref(),
        ))
    }

    fn build_action_service_method(
        &mut self,
        context: &mut GenerationContext,
        method_ident: &Ident,
        struct_prefix: &str,
        request_format: Option<&MessageFormat>,
        response_format: Option<&MessageFormat>,
        service_name: &str,
        schema_key: &str,
        response_struct_override: Option<&Ident>,
        response_context_label: Option<&str>,
        namespace: &str,
    ) -> Result<TokenStream> {
        let request_artifacts = map_message_format(request_format)?;
        let response_artifacts = map_message_format(response_format)?;

        let params = collect_function_params(
            request_artifacts.as_ref(),
            response_artifacts.as_ref(),
            struct_prefix,
            context,
            response_struct_override,
        )?;

        let method_label = method_ident.to_string();
        let method_label_literal = Literal::string(&method_label);
        let namespace_literal = Literal::string(namespace);
        let request_encoding = self.prepare_message_encoding(
            schema_key,
            struct_prefix,
            request_artifacts.as_ref(),
            &params,
        )?;

        let request_payload_tokens = if let Some(spec) = &request_encoding {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;

            quote! {
                let request_payload = {
                    let mut message = capnp::message::Builder::new_default();
                    {
                        let mut root = message.init_root::<#builder_type>();
                        #( #assignments )*
                    }
                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                        crate::Error::CapnpSerialize {
                            context: String::from(#method_label_literal),
                            source,
                        }
                    })?;
                    bytes::Bytes::from(buffer)
                };
            }
        } else {
            let suppress_unused = unused_params_stmt(&params);
            quote! {
                #suppress_unused
                let request_payload = bytes::Bytes::new();
            }
        };

        let service_name_literal = Literal::string(service_name);
        let poll_call = quote! {
            peppylib::ServiceMessenger::poll(
                messenger.handle(),
                "default_client",
                namespace,
                service_name,
                None,
                request_payload,
                timeout,
            )
        };

        let (return_ty, response_tokens, poll_tokens) = if let Some(response_artifacts) =
            response_artifacts.as_ref()
        {
            let response_struct_ident = response_struct_override.cloned().unwrap_or_else(|| {
                Ident::new(&format!("{struct_prefix}Response"), Span::call_site())
            });
            let response_struct_name = response_struct_ident.to_string();

            let response_schema_key = format!("{schema_key}_response");
            let response_schema = self.register_schema(
                &response_schema_key,
                &response_struct_name,
                response_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();

            let response_format = response_artifacts.message_format();
            let mut response_statements = Vec::new();
            let mut response_inits = Vec::new();
            let mut names = NameGenerator::new();

            for (field_name, schema) in &response_format.0 {
                let (mut statements, value_ident) = generate_field_reader_statements(
                    &quote!(root),
                    field_name,
                    schema,
                    &response_struct_name,
                    &response_struct_name,
                    &mut names,
                );
                response_statements.append(&mut statements);
                let field_ident =
                    Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                response_inits.push(quote!(#field_ident: #value_ident));
            }

            let response_context_value = response_context_label
                .map(str::to_string)
                .unwrap_or_else(|| response_struct_name.clone());
            let response_context_literal = Literal::string(&response_context_value);

            let poll_tokens = quote! {
                let response_bytes = #poll_call.await?;
            };

            let response_tokens = quote! {
                let mut cursor = std::io::Cursor::new(response_bytes.as_ref());
                let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: String::from(#response_context_literal),
                    source,
                })?;

                let root = message_reader
                    .get_root::<#reader_type>()
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: String::from(#response_context_literal),
                        source,
                    })?;

                #( #response_statements )*

                Ok(#response_struct_ident {
                    #( #response_inits ),*
                })
            };

            (quote!(#response_struct_ident), response_tokens, poll_tokens)
        } else {
            let poll_tokens = quote! {
                let _ = #poll_call.await?;
            };
            (quote!(()), quote!(Ok(())), poll_tokens)
        };

        let mut fn_params = vec![
            quote!(messenger: &crate::Messenger),
            quote!(timeout: std::time::Duration),
        ];
        fn_params.extend(function_param_tokens(&params));

        Ok(quote! {
            pub async fn #method_ident(#(#fn_params),*) -> crate::Result<#return_ty> {
                let namespace = #namespace_literal;
                let service_name = #service_name_literal;

                #request_payload_tokens

                #poll_tokens

                #response_tokens
            }
        })
    }

    fn build_action_cancel_method(
        &mut self,
        context: &mut GenerationContext,
        struct_prefix: &str,
        service_name: &str,
        schema_key: &str,
        response_context_label: Option<&str>,
        namespace: &str,
    ) -> Result<TokenStream> {
        let cancel_response_format = cancel_action_response_format();
        let cancel_response_ident = Ident::new("CancelResponse", Span::call_site());

        self.build_action_service_method(
            context,
            &Ident::new("cancel_goal", Span::call_site()),
            struct_prefix,
            None,
            Some(&cancel_response_format),
            service_name,
            schema_key,
            Some(&cancel_response_ident),
            response_context_label,
            namespace,
        )
    }

    fn build_action_feedback_method(
        &mut self,
        context: &mut GenerationContext,
        action: &SubscribedAction,
        format: &MessageFormat,
        action_struct_name: &str,
        feedback_context_label: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let format_artifacts = map_message_format(Some(format))?
            .expect("feedback format should always yield encoding artifacts");

        let struct_prefix = format!("{action_struct_name}Feedback");
        let schema_struct_name = format!("{struct_prefix}Message");
        let struct_ident = Ident::new("FeedbackMessage", Span::call_site());

        let params = collect_function_params(
            Some(&format_artifacts),
            None,
            &schema_struct_name,
            context,
            None,
        )?;

        let fields: Vec<(Ident, TokenStream)> = params
            .iter()
            .map(|param| (param.ident.clone(), param.ty.clone()))
            .collect();
        context.add_struct(struct_ident.clone(), fields);

        let schema_key = format!("{schema_struct_name}_payload");
        let encoding = self
            .prepare_message_encoding(
                &schema_key,
                &struct_prefix,
                Some(&format_artifacts),
                &params,
            )?
            .expect("feedback encoding spec should exist");
        let reader_type = encoding.reader_type.clone();

        let topic_name = action_endpoint_name(None, action.name.as_str(), "feedback");
        let topic_literal = Literal::string(&topic_name);
        let namespace_literal = Literal::string(action.node.as_str());

        let helper_fn_ident = Ident::new("deserialize_feedback_payload", Span::call_site());

        let format_schema = format_artifacts.message_format();
        let schema_lookup = SchemaFieldLookup::new(format_schema);

        let mut names = NameGenerator::new();
        let mut field_statements = Vec::new();
        let mut field_inits = Vec::new();
        for param in &params {
            let key = param.ident.to_string();
            let (original_name, schema) = schema_lookup.get(&key);
            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                original_name,
                schema,
                &schema_struct_name,
                feedback_context_label,
                &mut names,
            );
            field_statements.append(&mut statements);
            let field_ident = &param.ident;
            field_inits.push(quote!(#field_ident: #value_ident));
        }

        let context_literal = Literal::string(feedback_context_label);

        let helper_tokens = quote! {
            fn #helper_fn_ident(payload: &[u8]) -> crate::Result<#struct_ident> {
                let mut cursor = std::io::Cursor::new(payload);
                let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
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

                #( #field_statements )*

                Ok(#struct_ident {
                    #( #field_inits ),*
                })
            }
        };

        let method_ident = Ident::new("on_next_feedback_message", Span::call_site());
        let method_tokens = quote! {
            pub async fn #method_ident(
                messenger: &crate::Messenger,
            ) -> crate::Result<#struct_ident> {
                let topic_name = #topic_literal;
                let namespace = #namespace_literal;
                let qos = peppylib::config::QoSProfile::Standard;

                let message = {
                    let mut subscription = peppylib::TopicMessenger::listen(
                        messenger.handle(),
                        namespace,
                        topic_name,
                        qos,
                    )
                    .await
                    .map_err(|source| crate::Error::TopicSubscribe {
                        topic_name: topic_name.to_string(),
                        namespace: namespace.to_string(),
                        source,
                    })?;
                    subscription
                        .on_next_message()
                        .await
                        .ok_or_else(|| crate::Error::SubscriptionClosed {
                            topic_name: topic_name.to_string(),
                        })?
                };

                let payload = message.payload().as_bytes();
                #helper_fn_ident(payload.as_ref())
            }
        };

        Ok((method_tokens, vec![helper_tokens]))
    }
}

struct SchemaInfo {
    file_stem: String,
    struct_module: String,
}

impl SchemaInfo {
    fn builder_type_tokens(&self) -> TokenStream {
        let module_ident = Ident::new(&format!("{}_capnp", self.file_stem), Span::call_site());
        let struct_module_ident = Ident::new(&self.struct_module, Span::call_site());
        quote!(crate::capnp::#module_ident::#struct_module_ident::Builder)
    }

    fn reader_type_tokens(&self) -> TokenStream {
        let module_ident = Ident::new(&format!("{}_capnp", self.file_stem), Span::call_site());
        let struct_module_ident = Ident::new(&self.struct_module, Span::call_site());
        quote!(crate::capnp::#module_ident::#struct_module_ident::Reader)
    }
}

struct MessageEncodingSpec {
    builder_type: TokenStream,
    assignments: Vec<TokenStream>,
    reader_type: TokenStream,
}

struct ServiceResponseSpec<'a> {
    format: &'a MessageFormat,
    struct_ident: Ident,
    builder_type: TokenStream,
    include_service_instance_id: bool,
}

impl LanguageGenerator for RustGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();

        let schema_prefix = to_camel_case(&fn_name_str);
        let struct_prefix = String::from("Message");

        let mut context = GenerationContext::default();
        let format_artifacts = map_message_format(topic.message_format.as_ref())?;
        let params = collect_function_params(
            format_artifacts.as_ref(),
            None,
            &struct_prefix,
            &mut context,
            None,
        )?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &schema_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();
        let method_ident = Ident::new("emit", Span::call_site());
        let method_tokens = build_topic_emit(
            &method_ident,
            &params,
            encoding.as_ref(),
            topic,
            &fn_name_str,
        );

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*
            #method_tokens
        };
        let rendered = render_tokens(tokens);

        let mut module_label = topic.name.trim().to_string();
        if module_label.is_empty() {
            module_label = String::from("topic");
        }

        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::ExposedTopic,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let fn_name_str = fn_name.to_string();
        let struct_prefix = to_camel_case(&fn_name_str);
        let generic_response_ident = Ident::new("Response", Span::call_site());
        let generic_handler_ident = Ident::new("handle_next_request", Span::call_site());
        let generic_helper_ident = Ident::new("handle_request_payload", Span::call_site());
        let generic_deserializer_ident = Ident::new("deserialize_request", Span::call_site());

        let mut context = GenerationContext::default();
        let base_request_format = service
            .request_message_format
            .clone()
            .unwrap_or_else(|| MessageFormat(IndexMap::new()));
        let request_wire_artifacts = map_message_format(Some(&base_request_format))?
            .expect("request format should produce schema artifacts");
        let response_format = service.response_message_format.clone();
        let response_struct_artifacts = map_message_format(response_format.as_ref())?;
        let response_wire_artifacts = map_message_format(response_format.as_ref())?;
        let wire_params = collect_function_params(
            Some(&request_wire_artifacts),
            response_struct_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
            Some(&generic_response_ident),
        )?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &struct_prefix,
            Some(&request_wire_artifacts),
            &wire_params,
        )?;

        let handler_params = wire_params.clone();
        let instance_id_param =
            FunctionParam::new(Ident::new("instance_id", Span::call_site()), quote!(String));

        let request_struct_ident = if let Some((ident, tokens)) =
            build_request_struct_with_name("Request", &handler_params)
        {
            context.add_private_struct(tokens);
            Some(ident)
        } else {
            None
        };

        let response_spec = if let Some(return_artifacts) = response_wire_artifacts.as_ref() {
            let response_prefix = format!("{struct_prefix}Response");
            let schema_key = format!("{fn_name_str}_response");
            let schema_info =
                self.register_schema(&schema_key, &response_prefix, return_artifacts)?;
            Some(ServiceResponseSpec {
                format: return_artifacts.message_format(),
                struct_ident: generic_response_ident.clone(),
                builder_type: schema_info.builder_type_tokens(),
                include_service_instance_id: false,
            })
        } else {
            None
        };

        let service_name_literal = Literal::string(service.name.as_str());
        let (method_token, helper_tokens) = build_exposed_service_method(
            &fn_name,
            Some(&generic_handler_ident),
            Some(&generic_helper_ident),
            Some(&generic_deserializer_ident),
            &wire_params,
            &handler_params,
            Some(&instance_id_param),
            encoding.as_ref(),
            Some(request_wire_artifacts.message_format()),
            &fn_name_str,
            &service_name_literal,
            request_struct_ident.as_ref(),
            response_spec.as_ref(),
        );

        let mut service_tokens = context.into_tokens();
        service_tokens.push(method_token);
        service_tokens.extend(helper_tokens);

        let mut module_name = sanitize_component(service.name.as_str());
        if module_name.is_empty() {
            module_name = fn_name_str.clone();
        }

        let module = self
            .pending_exposed_services
            .get_or_insert_with(ExposedServicesModule::new);
        module.push_service(module_name, service_tokens);
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let base_ident = prefixed_ident("", non_empty_str(&action.name), "action");
        let base_name = base_ident.to_string();
        let action_prefix = to_camel_case(&base_name);

        let mut context = GenerationContext::default();
        let mut methods: Vec<TokenStream> = Vec::new();
        let mut helper_tokens: Vec<TokenStream> = Vec::new();

        if let Some(goal) = action.goal_service.as_ref() {
            let fn_name = Ident::new("goal", Span::call_site());
            let label = format!("{base_name}_goal");
            let schema_struct_prefix = format!("{action_prefix}Goal");
            let helper_ident = Ident::new("handle_goal_payload", Span::call_site());
            let service_name =
                action_endpoint_name(goal.name.as_deref(), action.name.as_str(), "goal");
            let service_literal = Literal::string(&service_name);

            let (method, helpers) = self.build_action_service_handler(
                &fn_name,
                None,
                Some(&helper_ident),
                "Goal",
                schema_struct_prefix.as_str(),
                goal.request_message_format.as_ref(),
                goal.response_message_format.as_ref(),
                &label,
                &service_literal,
                &mut context,
            )?;
            methods.push(method);
            helper_tokens.extend(helpers);

            let cancel_fn_name = Ident::new("cancel", Span::call_site());
            let cancel_label = format!("{base_name}_goal_cancel");
            let cancel_service_name = action_endpoint_name(None, action.name.as_str(), "cancel");
            let cancel_service_literal = Literal::string(&cancel_service_name);
            let cancel_struct_prefix = "Cancel";
            let cancel_schema_prefix = format!("{action_prefix}GoalCancel");
            let cancel_helper_ident =
                Ident::new("handle_cancel_request_payload", Span::call_site());
            let cancel_response_format = cancel_action_response_format();

            let (cancel_method, cancel_helpers) = self.build_action_service_handler(
                &cancel_fn_name,
                None,
                Some(&cancel_helper_ident),
                cancel_struct_prefix,
                cancel_schema_prefix.as_str(),
                None,
                Some(&cancel_response_format),
                &cancel_label,
                &cancel_service_literal,
                &mut context,
            )?;
            methods.push(cancel_method);
            helper_tokens.extend(cancel_helpers);
        }

        if let Some(feedback) = action.feedback_topic.as_ref() {
            let method_ident = Ident::new("emit_feedback", Span::call_site());
            let label = format!("emit_{base_name}_feedback");
            let struct_prefix = format!("{action_prefix}Feedback");
            let format_artifacts = map_message_format(feedback.message_format.as_ref())?;
            let params = collect_function_params(
                format_artifacts.as_ref(),
                None,
                &struct_prefix,
                &mut context,
                None,
            )?;
            let encoding = self.prepare_message_encoding(
                &label,
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            let topic_name =
                action_endpoint_name(feedback.name.as_deref(), action.name.as_str(), "feedback");
            let topic_descriptor = ExposedTopic {
                name: topic_name,
                qos_profile: feedback.qos_profile.clone(),
                message_format: feedback.message_format.clone(),
            };

            let method_tokens = build_topic_emit(
                &method_ident,
                &params,
                encoding.as_ref(),
                &topic_descriptor,
                &label,
            );
            methods.push(method_tokens);
        }

        if let Some(result) = action.result_service.as_ref() {
            let fn_name = Ident::new("result", Span::call_site());
            let label = format!("{base_name}_result");
            let schema_struct_prefix = format!("{action_prefix}Result");
            let helper_ident = Ident::new("handle_result_payload", Span::call_site());
            let service_name =
                action_endpoint_name(result.name.as_deref(), action.name.as_str(), "result");
            let service_literal = Literal::string(&service_name);

            let (method, helpers) = self.build_action_service_handler(
                &fn_name,
                None,
                Some(&helper_ident),
                "Result",
                schema_struct_prefix.as_str(),
                result.request_message_format.as_ref(),
                result.response_message_format.as_ref(),
                &label,
                &service_literal,
                &mut context,
            )?;
            methods.push(method);
            helper_tokens.extend(helpers);
        }

        if methods.is_empty() {
            return Ok(());
        }

        let mut items = context.into_tokens();
        items.extend(methods);
        items.extend(helper_tokens);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            rendered,
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: MessageFormat,
    ) -> Result<()> {
        let node_name = topic.node.as_str();

        let node_component = sanitize_component(node_name);
        let topic_component = sanitize_component(topic.name.as_str());

        debug_assert!(
            !node_component.is_empty(),
            "SubscribedTopic.node should be validated as non-empty"
        );
        debug_assert!(
            !topic_component.is_empty(),
            "SubscribedTopic.name should be validated as non-empty"
        );

        let node_prefix = to_camel_case(&node_component);
        let topic_prefix = to_camel_case(&topic_component);
        let mut struct_prefix = format!("{node_prefix}{topic_prefix}");
        if struct_prefix.is_empty() {
            struct_prefix = String::from("Topic");
        }

        let mut module_label = format!("{}_{}", node_name, topic.name.as_str());
        if module_label.trim().is_empty() {
            module_label = String::from("topic");
        }
        let mut module_component = sanitize_component(&module_label);
        if module_component.is_empty() {
            module_component = String::from("topic");
        }

        let schema_key = if !topic_component.is_empty() {
            format!("on_next_{topic_component}_message")
        } else if !node_component.is_empty() {
            format!("on_next_{node_component}_message")
        } else {
            format!("{module_component}_message")
        };

        let format_artifacts = map_message_format(Some(&arguments))?
            .expect("message encoding spec should exist when message format is provided");

        let mut context = GenerationContext::default();
        let message_struct_name = String::from("Message");
        let params = collect_function_params(
            Some(&format_artifacts),
            None,
            &message_struct_name,
            &mut context,
            None,
        )?;
        let encoding_params = params.clone();

        let args_struct_ident = Ident::new(&message_struct_name, Span::call_site());
        let args_fields: Vec<(Ident, TokenStream)> = params
            .iter()
            .map(|param| (param.ident.clone(), param.ty.clone()))
            .collect();
        context.add_struct(args_struct_ident.clone(), args_fields);

        let callback_fn_ident = Ident::new("on_next_message_received", Span::call_site());
        let helper_fn_ident = Ident::new("deseralize_payload", Span::call_site());

        let encoding = self
            .prepare_message_encoding(
                &schema_key,
                &struct_prefix,
                Some(&format_artifacts),
                &encoding_params,
            )?
            .expect("message encoding spec should exist when message format is provided");
        let method_tokens = build_subscribed_topic_callback(
            &callback_fn_ident,
            &helper_fn_ident,
            &args_struct_ident,
            &params,
            &format_artifacts,
            &encoding,
            topic,
            &message_struct_name,
        );
        let mut items = context.into_tokens();
        items.push(method_tokens);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedTopic,
            rendered,
        ));

        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        request_arguments: Option<&MessageFormat>,
        response_arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        let request_artifacts = map_message_format(request_arguments)?;
        let response_artifacts = map_message_format(response_arguments)?;

        let service_ident = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let service_name_component = service_ident.to_string();
        let struct_prefix = to_camel_case(service_name_component.as_str());
        let service_label = subscribed_service_label(service);
        let request_context_label = format!("poll {service_label}");
        let response_context_label = format!("{service_label} response");

        let method_label = {
            let mut components = Vec::with_capacity(3);
            components.push(String::from("poll"));

            let node_ident = prefixed_ident("", Some(service.node.as_str()), "node");
            components.push(node_ident.to_string());

            components.push(service_name_component.clone());

            components.join("_")
        };
        let method_ident = Ident::new("poll", Span::call_site());

        let mut context = GenerationContext::default();
        let generic_response_ident = Ident::new("Response", Span::call_site());

        let params = collect_function_params(
            request_artifacts.as_ref(),
            response_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
            Some(&generic_response_ident),
        )?;

        let instance_id_param_index = params
            .iter()
            .position(|param| param.ident.to_string() == "instance_id");

        let mut request_struct_params = params.clone();
        if let Some(index) = instance_id_param_index {
            request_struct_params.remove(index);
        }

        let request_struct_ident = Ident::new("Request", Span::call_site());
        if !request_struct_params.is_empty() {
            let fields = request_struct_params
                .iter()
                .map(|param| (param.ident.clone(), param.ty.clone()))
                .collect();
            context.add_struct(request_struct_ident.clone(), fields);
            let ctor_params: Vec<TokenStream> = request_struct_params
                .iter()
                .map(|param| {
                    let ident = &param.ident;
                    let ty = &param.ty;
                    quote!(#ident: #ty)
                })
                .collect();
            let ctor_bindings: Vec<TokenStream> = request_struct_params
                .iter()
                .map(|param| {
                    let ident = &param.ident;
                    quote!(#ident)
                })
                .collect();
            let ctor_tokens = quote! {
                impl #request_struct_ident {
                    pub fn new(#(#ctor_params),*) -> Self {
                        Self {
                            #( #ctor_bindings ),*
                        }
                    }
                }
            };
            context.add_private_struct(ctor_tokens);
        }

        let request_encoding = self.prepare_message_encoding(
            &method_label,
            &struct_prefix,
            request_artifacts.as_ref(),
            &params,
        )?;

        let request_context_literal = Literal::string(&request_context_label);

        let request_payload_tokens = if let Some(spec) = &request_encoding {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;

            let unpacking = request_struct_params.iter().map(|p| {
                let ident = &p.ident;
                quote!(let #ident = request.#ident;)
            });

            quote! {
                let request_payload = {
                    let mut message = capnp::message::Builder::new_default();
                    {
                        let mut root = message.init_root::<#builder_type>();
                        #( #unpacking )*
                        #( #assignments )*
                    }
                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                        crate::Error::CapnpSerialize {
                            context: String::from(#request_context_literal),
                            source,
                        }
                    })?;
                    bytes::Bytes::from(buffer)
                };
            }
        } else {
            let suppress_unused = unused_params_stmt(&params);

            quote! {
                #suppress_unused
                let request_payload = bytes::Bytes::new();
            }
        };

        let poll_call = quote! {
            peppylib::ServiceMessenger::poll(
                messenger.handle(),
                instance_id.as_str(),
                node_name,
                service_name,
                final_target_instance_id.as_deref(),
                request_payload,
                timeout,
            )
        };

        let (return_ty, response_tokens, poll_tokens) =
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                let response_struct_name = format!("{struct_prefix}Response");
                let response_struct_ident = generic_response_ident.clone();

                let response_schema_key = format!("{method_label}_response");
                let response_schema = self.register_schema(
                    &response_schema_key,
                    &response_struct_name,
                    response_artifacts,
                )?;
                let response_reader_type = response_schema.reader_type_tokens();

                let response_format = response_artifacts.message_format();
                let schema_lookup = SchemaFieldLookup::new(response_format);
                let mut response_statements = Vec::new();
                let mut response_inits = Vec::new();
                let mut name_gen = NameGenerator::new();
                for (field_name, _) in &response_format.0 {
                    let (original_name, schema) = schema_lookup.get(field_name);
                    let (mut statements, value_ident) = generate_field_reader_statements(
                        &quote!(root),
                        original_name.as_str(),
                        schema,
                        &response_struct_name,
                        &response_context_label,
                        &mut name_gen,
                    );
                    response_statements.append(&mut statements);
                    let field_ident =
                        Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                    response_inits.push(quote!(#field_ident: #value_ident));
                }

                let response_context_literal = Literal::string(&response_context_label);

                let poll_tokens = quote! {
                    let response_message = #poll_call.await?;
                };

                let response_tokens = quote! {
                    let response_instance_id = response_message
                        .instance_id()
                        .map(str::to_string)
                        .ok_or_else(|| {
                            crate::Error::MissingInstanceId {
                                key_expr: response_message.key_expr().to_string(),
                            }
                        })?;

                    let mut cursor = std::io::Cursor::new(response_message.payload().as_bytes());
                    let message_reader = capnp::serialize::read_message(
                        &mut cursor,
                        capnp::message::ReaderOptions::new(),
                    )
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: String::from(#response_context_literal),
                        source,
                    })?;

                    let root = message_reader
                        .get_root::<#response_reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#response_context_literal),
                            source,
                        })?;

                    #( #response_statements )*

                    Ok((
                        response_instance_id,
                        #response_struct_ident {
                            #( #response_inits ),*
                        },
                    ))
                };

                (
                    quote!((String, #response_struct_ident)),
                    response_tokens,
                    poll_tokens,
                )
            } else {
                let poll_tokens = quote! {
                    let _ = #poll_call.await?;
                };
                (quote!(()), quote!(Ok(())), poll_tokens)
            };

        let mut service_tokens = context.into_tokens();
        let service_name_literal = Literal::string(service.name.as_str());
        let node_name_literal = Literal::string(service.node.as_str());

        let mut fn_param_tokens = vec![
            quote!(messenger: &crate::Messenger),
            quote!(timeout: std::time::Duration),
            quote!(target_instance_id: Option<String>),
        ];
        if !request_struct_params.is_empty() {
            fn_param_tokens.push(quote!(request: #request_struct_ident));
        }

        let function_token = quote! {
            /// Ignores the target_instance_id argument if it has already been set by a deployment
            pub async fn #method_ident(#(#fn_param_tokens),*) -> crate::Result<#return_ty> {
                let node_name = #node_name_literal;
                let service_name = #service_name_literal;

                let instance_id = std::env::var("PEPPY_INSTANCE_ID")
                    .map_err(|source| {
                        crate::Error::MissingInstanceIdEnvVar {
                            var: "PEPPY_INSTANCE_ID",
                            source,
                        }
                    })?;
                let deployment_target_instance_id = std::env::var(format!(
                    "PEPPY_{}_{}_TARGET_INSTANCE_ID",
                    &node_name, &service_name
                ))
                .ok();
                let final_target_instance_id =
                    deployment_target_instance_id.or(target_instance_id);

                #request_payload_tokens

                #poll_tokens

                #response_tokens
            }
        };

        service_tokens.push(function_token);

        let mut module_name = subscribed_service_module_name(service);
        if module_name.is_empty() {
            module_name = method_label
                .strip_prefix("poll_")
                .map(|label| label.to_string())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| method_label.clone());
        }

        let tokens: TokenStream = quote! {
            #( #service_tokens )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(InterfaceArtifact::from_kind(
            &module_name,
            InterfaceKind::SubscribedService,
            rendered,
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: Option<&SubscribedActionMessage>,
    ) -> Result<()> {
        let messages = messages
            .ok_or_else(|| Error::SubscriberActionMessageFormatMissing(action.name.clone()))?;

        let node_component = sanitize_component(action.node.as_str());
        let action_component = sanitize_component(action.name.as_str());
        let base_component = match (node_component.is_empty(), action_component.is_empty()) {
            (true, true) => "action".to_string(),
            (true, false) => action_component.clone(),
            (false, true) => node_component.clone(),
            (false, false) => format!("{node_component}_{action_component}"),
        };
        let action_prefix = to_camel_case(&base_component);
        let action_struct_name = format!("{action_prefix}Action");
        let action_context_label =
            subscribed_action_context_label(action.node.as_str(), action.name.as_str());

        let mut context = GenerationContext::default();
        let mut methods: Vec<TokenStream> = Vec::new();
        let mut helper_items: Vec<TokenStream> = Vec::new();

        let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
        let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
        let feedback_format = non_empty_message_format(messages.feedback.as_ref());
        let result_request_format = non_empty_message_format(messages.result_request.as_ref());
        let result_response_format = non_empty_message_format(messages.result_response.as_ref());

        let goal_response_ident = Ident::new("GoalResponse", Span::call_site());
        let goal_response_override = if goal_response_format.is_some() {
            Some(&goal_response_ident)
        } else {
            None
        };
        let goal_response_context = goal_response_format
            .is_some()
            .then(|| format!("{action_context_label} GoalResponse"));
        let goal_schema_key = format!("{action_struct_name}_fire_goal");

        let goal_method = self.build_action_service_method(
            &mut context,
            &Ident::new("fire_goal", Span::call_site()),
            &format!("{action_struct_name}Goal"),
            goal_request_format,
            goal_response_format,
            &action_endpoint_name(None, action.name.as_str(), "goal"),
            &goal_schema_key,
            goal_response_override,
            goal_response_context.as_deref(),
            action.node.as_str(),
        )?;
        methods.push(goal_method);

        let cancel_struct_prefix = format!("{action_struct_name}Cancel");
        let cancel_schema_key = format!("{action_struct_name}_cancel_goal");
        let cancel_service_name = action_endpoint_name(None, action.name.as_str(), "cancel");
        let cancel_response_context = Some(format!("{action_context_label} CancelResponse"));
        let cancel_method = self.build_action_cancel_method(
            &mut context,
            &cancel_struct_prefix,
            &cancel_service_name,
            &cancel_schema_key,
            cancel_response_context.as_deref(),
            action.node.as_str(),
        )?;
        methods.push(cancel_method);

        if let Some(feedback_format) = feedback_format {
            let feedback_context_label = format!("{action_context_label} FeedbackMessage");
            let (feedback_method, mut feedback_helpers) = self.build_action_feedback_method(
                &mut context,
                action,
                feedback_format,
                &action_struct_name,
                &feedback_context_label,
            )?;
            methods.push(feedback_method);
            helper_items.append(&mut feedback_helpers);
        }

        let result_response_ident = Ident::new("ResultResponse", Span::call_site());
        let result_response_override = if result_response_format.is_some() {
            Some(&result_response_ident)
        } else {
            None
        };
        let result_response_context = result_response_format
            .is_some()
            .then(|| format!("{action_context_label} ResultResponse"));
        let result_schema_key = format!("{action_struct_name}_get_action_result");

        let result_method = self.build_action_service_method(
            &mut context,
            &Ident::new("get_action_result", Span::call_site()),
            &format!("{action_struct_name}Result"),
            result_request_format,
            result_response_format,
            &action_endpoint_name(None, action.name.as_str(), "result"),
            &result_schema_key,
            result_response_override,
            result_response_context.as_deref(),
            action.node.as_str(),
        )?;
        methods.push(result_method);

        let mut items = context.into_tokens();
        items.extend(methods);
        items.extend(helper_items);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);
        let module_label = subscribed_action_module_name(action);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedAction,
            rendered,
        ));
        Ok(())
    }

    fn build(mut self, to_path: impl AsRef<Path>) -> Result<()> {
        self.flush_pending_exposed_services();

        // First create the basic structure of the project
        common::add_peppylib_dependencies(&to_path)?;
        // Write the schema files to the project
        common::write_capnp_schemas(&self.schemas, to_path.as_ref())?;
        // Add the content to the Rust files
        common::add_artifacts_to_lib(&to_path, self.sections)?;

        // TODO: The services should start when the node starts, there should be a way to pass in the callbacks for the service to start in peppygen
        // add_startup_services();

        let crate_root = to_path.as_ref();
        let node_config_path = crate_root.join(PEPPY_NODE_CONFIG_FILE);
        // Lastly generate the codegen fingerprint based on the peppy.json5 config file
        checker::generate_node_config_fingerprint(&node_config_path, crate_root)?;
        Ok(())
    }
}

struct ServiceModule {
    module_name: String,
    tokens: Vec<TokenStream>,
}

struct ExposedServicesModule {
    services: Vec<ServiceModule>,
}

impl ExposedServicesModule {
    fn new() -> Self {
        Self {
            services: Vec::new(),
        }
    }

    fn push_service(&mut self, module_name: String, tokens: Vec<TokenStream>) {
        self.services.push(ServiceModule {
            module_name,
            tokens,
        });
    }

    fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.services
            .into_iter()
            .map(|service| {
                let ServiceModule {
                    module_name,
                    tokens,
                } = service;
                let tokens: TokenStream = quote! {
                    #( #tokens )*
                };

                let rendered = render_tokens(tokens);

                InterfaceArtifact::from_kind(&module_name, InterfaceKind::ExposedService, rendered)
            })
            .collect()
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn subscribed_service_label(service: &SubscribedService) -> String {
    let mut label = String::new();
    let node = service.node.trim();
    if !node.is_empty() {
        label.push_str(node);
    }
    let name = service.name.trim();
    if !name.is_empty() {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(name);
    }

    if label.is_empty() {
        "service".to_string()
    } else {
        label
    }
}

fn subscribed_service_module_name(service: &SubscribedService) -> String {
    let node_component = sanitize_component(service.node.as_str());
    let service_component = sanitize_component(service.name.as_str());

    match (node_component.is_empty(), service_component.is_empty()) {
        (false, false) => format!("{node_component}_{service_component}"),
        (false, true) => node_component,
        (true, false) => service_component,
        (true, true) => String::new(),
    }
}

fn subscribed_action_module_name(action: &SubscribedAction) -> String {
    let node_component = sanitize_component(action.node.as_str());
    let action_component = sanitize_component(action.name.as_str());

    match (node_component.is_empty(), action_component.is_empty()) {
        (false, false) => format!("{node_component}_{action_component}"),
        (false, true) => node_component,
        (true, false) => action_component,
        (true, true) => String::new(),
    }
}

fn non_empty_message_format<'a>(format: Option<&'a MessageFormat>) -> Option<&'a MessageFormat> {
    format.filter(|format| !format.0.is_empty())
}

fn cancel_action_response_format() -> MessageFormat {
    let mut fields = IndexMap::new();
    fields.insert(String::from("accepted"), SchemaType::Type(TypeToken::Bool));
    fields.insert(
        String::from("error_message"),
        SchemaType::Primitive(PrimitiveSchema {
            kind: TypeToken::String,
            optional: true,
        }),
    );

    MessageFormat(fields)
}

fn action_endpoint_name(custom: Option<&str>, action_name: &str, suffix: &str) -> String {
    if let Some(candidate) = custom {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let trimmed_action = action_name.trim();
    if trimmed_action.is_empty() {
        suffix.to_string()
    } else if trimmed_action.ends_with('/') {
        format!("{trimmed_action}{suffix}")
    } else {
        format!("{trimmed_action}/{suffix}")
    }
}

fn subscribed_action_context_label(node: &str, action_name: &str) -> String {
    let node = node.trim();
    let action = action_name.trim();

    match (node.is_empty(), action.is_empty()) {
        (true, true) => String::from("action"),
        (true, false) => action.to_string(),
        (false, true) => node.to_string(),
        (false, false) => format!("{node} {action}"),
    }
}

fn prefixed_ident(prefix: &str, candidate: Option<&str>, fallback: &str) -> Ident {
    let fallback_component = match sanitize_component(fallback) {
        component if component.is_empty() => "item".to_string(),
        component => component,
    };

    let maybe_component = candidate.and_then(|value| {
        let sanitized = sanitize_component(value);
        if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        }
    });

    let component = maybe_component
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_component.clone());

    let name = if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
    };

    Ident::new(&name, Span::call_site())
}

fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;

    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_underscore = false;
        } else if !out.is_empty() && !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        } else if out.is_empty() {
            last_was_underscore = true;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        return String::new();
    }

    if matches!(out.chars().next(), Some(c) if c.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
}

fn normalize_snake_case(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    let mut prev_was_lower_or_digit = false;

    while let Some(ch) = chars.next() {
        if ch.is_ascii_uppercase() {
            let next_is_lower = chars
                .peek()
                .copied()
                .map(|next| next.is_ascii_lowercase())
                .unwrap_or(false);
            if !result.is_empty()
                && (prev_was_lower_or_digit || next_is_lower)
                && !result.ends_with('_')
            {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_was_lower_or_digit = true;
        } else if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            result.push(ch);
            prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !result.ends_with('_') && !result.is_empty() {
                result.push('_');
            }
            prev_was_lower_or_digit = false;
        }
    }

    while result.ends_with('_') {
        result.pop();
    }

    if result.is_empty() {
        return String::from("message");
    }

    if result
        .chars()
        .next()
        .map(|ch| ch.is_ascii_digit())
        .unwrap_or(false)
    {
        result.insert(0, '_');
    }

    result
}

#[derive(Default)]
struct GenerationContext {
    structs: Vec<StructDefinition>,
    private_items: Vec<TokenStream>,
}

impl GenerationContext {
    fn add_struct(&mut self, ident: Ident, fields: Vec<(Ident, TokenStream)>) {
        if let Some(existing) = self.structs.iter_mut().find(|def| def.ident == ident) {
            *existing = StructDefinition { ident, fields };
        } else {
            self.structs.push(StructDefinition { ident, fields });
        }
    }

    fn add_private_struct(&mut self, tokens: TokenStream) {
        self.private_items.push(tokens);
    }

    fn wrap_optional_type(&mut self, ty: TokenStream) -> TokenStream {
        quote!(Option<#ty>)
    }

    fn into_tokens(self) -> Vec<TokenStream> {
        let mut items: Vec<TokenStream> = Vec::new();
        items.extend(self.structs.into_iter().map(StructDefinition::into_tokens));
        items.extend(self.private_items);
        items
    }
}

struct StructDefinition {
    ident: Ident,
    fields: Vec<(Ident, TokenStream)>,
}

impl StructDefinition {
    fn into_tokens(self) -> TokenStream {
        let ident = self.ident;
        let field_tokens: Vec<TokenStream> = self
            .fields
            .into_iter()
            .map(|(field_ident, ty)| {
                let name = field_ident;
                let field_ty = ty;
                quote!(pub #name: #field_ty)
            })
            .collect();

        if field_tokens.is_empty() {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct #ident {}
            }
        } else {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct #ident {
                    #( #field_tokens ),*
                }
            }
        }
    }
}

fn map_message_format(format: Option<&MessageFormat>) -> Result<Option<CapnpSchemaArtifacts>> {
    match format {
        Some(format) => MessageFormatMapper::new(format.clone())
            .map_message_format_to_capnpn()
            .map(Some)
            .map_err(Error::MessageEncoding),
        None => Ok(None),
    }
}

struct SchemaFieldLookup<'a> {
    entries: HashMap<String, (&'a String, &'a SchemaType)>,
}

impl<'a> SchemaFieldLookup<'a> {
    fn new(format: &'a MessageFormat) -> Self {
        let mut entries = HashMap::with_capacity(format.0.len() * 2);
        for (name, schema) in &format.0 {
            let capnp_key = sanitize_capnp_field_name(name);
            entries.insert(capnp_key, (name, schema));

            let rust_key = sanitize_component(name);
            entries.entry(rust_key).or_insert((name, schema));
        }
        Self { entries }
    }

    fn get(&self, key: &str) -> (&'a String, &'a SchemaType) {
        *self
            .entries
            .get(key)
            .unwrap_or_else(|| panic!("missing schema entry for field `{key}`"))
    }
}

fn collect_function_params(
    accept_format_artifacts: Option<&CapnpSchemaArtifacts>,
    return_format_artifacts: Option<&CapnpSchemaArtifacts>,
    struct_prefix: &str,
    context: &mut GenerationContext,
    response_struct_name_override: Option<&Ident>,
) -> Result<Vec<FunctionParam>> {
    if let Some(return_artifacts) = return_format_artifacts {
        let response_struct_name = response_struct_name_override
            .map(|ident| ident.to_string())
            .unwrap_or_else(|| format!("{struct_prefix}Response"));
        let response_ident = Ident::new(&response_struct_name, Span::call_site());
        let mut fields = Vec::new();
        let mut ctor_params: Vec<TokenStream> = Vec::new();
        let mut ctor_bindings: Vec<TokenStream> = Vec::new();

        for (field_name, schema) in &return_artifacts.message_format().0 {
            let field_ident =
                Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
            let field_ty =
                schema_type_to_tokens(schema, &response_struct_name, field_name, context);
            let ctor_ident = field_ident.clone();
            let ctor_ty = field_ty.clone();
            ctor_params.push(quote!(#ctor_ident: #ctor_ty));
            ctor_bindings.push(quote!(#ctor_ident));
            fields.push((field_ident, field_ty));
        }

        context.add_struct(response_ident.clone(), fields);

        let ctor_tokens = if ctor_params.is_empty() {
            quote! {
                impl #response_ident {
                    pub fn new() -> Self {
                        Self {}
                    }
                }
            }
        } else {
            quote! {
                impl #response_ident {
                    pub fn new(#(#ctor_params),*) -> Self {
                        Self {
                            #( #ctor_bindings ),*
                        }
                    }
                }
            }
        };
        context.add_private_struct(ctor_tokens);
    }

    let Some(artifacts) = accept_format_artifacts else {
        return Ok(Vec::new());
    };

    let format = artifacts.message_format();
    let schema_lookup = SchemaFieldLookup::new(format);
    let capnp_params = artifacts
        .build_function_params()
        .map_err(Error::MessageEncoding)?;

    let mut params = Vec::with_capacity(capnp_params.len());
    for param in capnp_params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&key);

        let ident = Ident::new(&sanitize_component(original_name), Span::call_site());
        let ty = schema_type_to_tokens(schema, struct_prefix, original_name, context);

        params.push(FunctionParam::new(ident, ty));
    }

    Ok(params)
}

fn schema_type_to_tokens(
    schema: &SchemaType,
    struct_prefix: &str,
    field_name: &str,
    context: &mut GenerationContext,
) -> TokenStream {
    let ty = match schema {
        SchemaType::Type(token) => primitive_type_token(token),
        SchemaType::Primitive(primitive) => primitive_type_token(&primitive.kind),
        SchemaType::Array(array) => {
            let item_ty = match array.items.as_ref().as_type_token() {
                Some(token) => primitive_type_token(token),
                None => panic!("unsupported nested schema type in array `{field_name}`"),
            };

            if let Some(length) = array.length {
                let len_lit = Literal::usize_unsuffixed(length);
                quote!([#item_ty; #len_lit])
            } else {
                quote!(Vec<#item_ty>)
            }
        }
        SchemaType::Object(object) => {
            let struct_name = format!("{struct_prefix}{}", to_camel_case(field_name));
            let struct_ident = Ident::new(&struct_name, Span::call_site());

            let mut fields = Vec::with_capacity(object.fields.len());
            for (nested_name, nested_schema) in &object.fields {
                let field_ident =
                    Ident::new(&sanitize_component(nested_name.as_str()), Span::call_site());
                let field_ty =
                    schema_type_to_tokens(nested_schema, &struct_name, nested_name, context);
                fields.push((field_ident, field_ty));
            }

            context.add_struct(struct_ident.clone(), fields);
            quote!(#struct_ident)
        }
    };

    if schema.is_optional() {
        context.wrap_optional_type(ty)
    } else {
        ty
    }
}

fn primitive_type_token(token: &TypeToken) -> TokenStream {
    match token {
        TypeToken::Bool => quote!(bool),
        TypeToken::String => quote!(String),
        TypeToken::Bytes => quote!(Vec<u8>),
        TypeToken::Time => quote!(std::time::SystemTime),
        TypeToken::U8 => quote!(u8),
        TypeToken::U16 => quote!(u16),
        TypeToken::U32 => quote!(u32),
        TypeToken::U64 => quote!(u64),
        TypeToken::I8 => quote!(i8),
        TypeToken::I16 => quote!(i16),
        TypeToken::I32 => quote!(i32),
        TypeToken::I64 => quote!(i64),
        TypeToken::F32 => quote!(f32),
        TypeToken::F64 => quote!(f64),
    }
}

fn sanitize_capnp_field_name(input: &str) -> String {
    fn to_pascal_case(input: &str) -> String {
        let mut result = String::new();

        for segment in input
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
        {
            let mut chars = segment.chars();
            if let Some(first) = chars.next() {
                result.push(first.to_ascii_uppercase());
                for ch in chars {
                    result.push(ch.to_ascii_lowercase());
                }
            }
        }

        result
    }

    let mut output = to_pascal_case(input);
    if output.is_empty() {
        output.push_str("Field");
    }

    let mut chars = output.chars();
    let mut camel = String::with_capacity(output.len());
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().map_or(false, |ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    if camel.is_empty() {
        "_field".to_string()
    } else {
        camel
    }
}

fn unused_params_stmt(params: &[FunctionParam]) -> TokenStream {
    if params.is_empty() {
        TokenStream::new()
    } else if params.len() == 1 {
        let ident = &params[0].ident;
        quote! {
            let _ = &#ident;
        }
    } else {
        let refs: Vec<TokenStream> = params
            .iter()
            .map(|param| {
                let ident = &param.ident;
                quote!(&#ident)
            })
            .collect();
        quote! {
            let _ = (#(#refs),*);
        }
    }
}

fn render_tokens(tokens: TokenStream) -> String {
    parse2::<File>(tokens.clone())
        .map(|file| prettyplease::unparse(&file))
        .unwrap_or_else(|_| tokens.to_string())
}

fn generate_assignments_for_format(
    builder_ident: &Ident,
    format: &MessageFormat,
    params: &[FunctionParam],
) -> Vec<TokenStream> {
    let mut param_lookup: HashMap<String, Ident> = HashMap::new();
    for param in params {
        param_lookup.insert(param.ident.to_string(), param.ident.clone());
    }

    let mut assignments = Vec::with_capacity(format.0.len());
    let mut name_gen = NameGenerator::new();
    let builder_expr = quote!(#builder_ident);

    for (field_name, schema) in &format.0 {
        let sanitized = sanitize_component(field_name);
        let param_ident = param_lookup
            .get(&sanitized)
            .unwrap_or_else(|| panic!("missing parameter for field `{field_name}`"))
            .clone();
        let value_expr = quote!(#param_ident);
        assignments.push(generate_field_assignment(
            &builder_expr,
            field_name,
            schema,
            &value_expr,
            &mut name_gen,
        ));
    }

    assignments
}

fn generate_field_assignment(
    builder_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> TokenStream {
    generate_field_assignment_inner(
        builder_expr,
        field_name,
        schema,
        value_expr,
        names,
        true,
        false,
    )
}

fn generate_field_assignment_inner(
    builder_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
    handle_optional: bool,
    value_is_ref: bool,
) -> TokenStream {
    if handle_optional && schema.is_optional() {
        match schema {
            SchemaType::Object(_) => {
                let binding = names.next("value");
                let inner_expr = quote!(#binding);
                let inner = generate_field_assignment_inner(
                    builder_expr,
                    field_name,
                    schema,
                    &inner_expr,
                    names,
                    false,
                    false,
                );
                return quote! {
                    if let Some(#binding) = (#value_expr).cloned() {
                        #inner
                    }
                };
            }
            _ => {
                let binding = names.next("value");
                let inner_expr = quote!(#binding);
                let inner = generate_field_assignment_inner(
                    builder_expr,
                    field_name,
                    schema,
                    &inner_expr,
                    names,
                    false,
                    true,
                );
                return quote! {
                    if let Some(#binding) = (#value_expr).as_ref() {
                        #inner
                    }
                };
            }
        }
    }

    let method_component = sanitize_component(field_name);
    let set_method = Ident::new(&format!("set_{method_component}"), Span::call_site());
    let init_method = Ident::new(&format!("init_{method_component}"), Span::call_site());

    match schema {
        SchemaType::Type(token) => primitive_field_assignment(
            builder_expr,
            &set_method,
            &init_method,
            value_expr,
            names,
            value_is_ref,
            token,
        ),
        SchemaType::Primitive(primitive) => primitive_field_assignment(
            builder_expr,
            &set_method,
            &init_method,
            value_expr,
            names,
            value_is_ref,
            &primitive.kind,
        ),
        SchemaType::Array(array) => {
            let item_token = array.items.as_ref().as_type_token();
            if matches!(item_token, Some(TypeToken::U8)) {
                quote!(#builder_expr.#set_method(#value_expr.as_ref());)
            } else if let Some(token) = item_token {
                generate_list_assignment(
                    builder_expr,
                    &init_method,
                    value_expr,
                    array.length,
                    token,
                    names,
                )
            } else {
                panic!("unsupported nested schema type in array `{field_name}`");
            }
        }
        SchemaType::Object(object) => generate_object_assignment(
            builder_expr,
            &init_method,
            value_expr,
            &object.fields,
            names,
        ),
    }
}

fn primitive_field_assignment(
    builder_expr: &TokenStream,
    set_method: &Ident,
    init_method: &Ident,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
    value_is_ref: bool,
    token: &TypeToken,
) -> TokenStream {
    match token {
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => {
            let value_tokens = if value_is_ref {
                quote!(*#value_expr)
            } else {
                quote!(#value_expr)
            };
            quote!(#builder_expr.#set_method(#value_tokens);)
        }
        TypeToken::String => {
            quote!(#builder_expr.#set_method(#value_expr.as_str());)
        }
        TypeToken::Bytes => {
            quote!(#builder_expr.#set_method(#value_expr.as_ref());)
        }
        TypeToken::Time => {
            let time_expr = if value_is_ref {
                quote!(*#value_expr)
            } else {
                quote!(#value_expr)
            };
            generate_time_assignment(builder_expr, init_method, &time_expr, names)
        }
    }
}

fn generate_time_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> TokenStream {
    let timestamp_ident = names.next("timestamp");
    let builder_ident = names.next("timestamp_builder");

    quote! {
        let #timestamp_ident = peppylib::encoding::convert_time(#value_expr);
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #builder_ident.set_sec(#timestamp_ident.sec);
        #builder_ident.set_nsec(#timestamp_ident.nsec);
    }
}

fn generate_list_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    length: Option<usize>,
    token: &TypeToken,
    names: &mut NameGenerator,
) -> TokenStream {
    let list_ident = names.next("list");
    let idx_ident = names.next("idx");
    let element_ident = names.next("value");

    let length_expr = match length {
        Some(len) => {
            let len_lit = Literal::u32_unsuffixed(len as u32);
            quote!(#len_lit)
        }
        None => quote!((#value_expr).len() as u32),
    };

    let element_setter = match token {
        TypeToken::String => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_str());),
        TypeToken::Bytes => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_ref());),
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => quote!(#list_ident.set(#idx_ident as u32, *#element_ident);),
        TypeToken::Time => panic!("time arrays are not supported"),
    };

    quote! {
        let mut #list_ident = #builder_expr.reborrow().#init_method(#length_expr);
        for (#idx_ident, #element_ident) in (#value_expr).iter().enumerate() {
            #element_setter
        }
    }
}

fn generate_object_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    fields: &IndexMap<String, SchemaType>,
    names: &mut NameGenerator,
) -> TokenStream {
    let builder_ident = names.next("builder");
    let mut nested = Vec::with_capacity(fields.len());

    for (nested_name, nested_schema) in fields {
        let nested_ident = Ident::new(&sanitize_component(nested_name.as_str()), Span::call_site());
        let nested_value_expr = quote!(#value_expr.#nested_ident);
        nested.push(generate_field_assignment(
            &quote!(#builder_ident),
            nested_name,
            nested_schema,
            &nested_value_expr,
            names,
        ));
    }

    quote! {
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #(#nested)*
    }
}

#[derive(Default)]
struct NameGenerator {
    counter: usize,
}

impl NameGenerator {
    fn new() -> Self {
        Self { counter: 0 }
    }

    fn next(&mut self, hint: &str) -> Ident {
        let sanitized = sanitize_component(hint);
        let suffix = self.counter;
        self.counter += 1;
        let base = if sanitized.is_empty() {
            "tmp".to_string()
        } else {
            sanitized
        };
        Ident::new(&format!("{base}_{suffix}"), Span::call_site())
    }
}

fn build_topic_emit(
    method_ident: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    topic: &ExposedTopic,
    label: &str,
) -> TokenStream {
    let mut method_param_tokens = Vec::new();
    for param in params {
        if param.ident.to_string() == "instance_id" {
            continue;
        }
        let ident = &param.ident;
        let ty = &param.ty;
        method_param_tokens.push(quote!(#ident: #ty));
    }

    let method_signature = if method_param_tokens.is_empty() {
        quote!(messenger: &crate::Messenger)
    } else {
        quote!(messenger: &crate::Messenger, #(#method_param_tokens),*)
    };
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);
    let instance_id_ident = Ident::new("instance_id", Span::call_site());
    let env_var_literal = Literal::string("PEPPY_INSTANCE_ID");
    let instance_id_stmt = quote! {
        let #instance_id_ident = std::env::var(#env_var_literal).map_err(|source| {
            crate::Error::MissingInstanceIdEnvVar {
                var: #env_var_literal,
                source,
            }
        })?;
    };

    match encoding {
        Some(spec) => {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let root_ident = Ident::new("root", Span::call_site());

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_ident(#method_signature) -> crate::Result<()> {
                    #instance_id_stmt
                    let mut message = capnp::message::Builder::new_default();
                    {
                        let mut #root_ident = message.init_root::<#builder_type>();
                        #(#assignments)*
                    }

                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                        crate::Error::CapnpSerialize {
                            context: String::from(#label_literal),
                            source,
                        }
                    })?;

                    let payload = bytes::Bytes::from(buffer);
                    let topic_name = #topic_literal;
                    let qos = #qos_tokens;

                    peppylib::TopicMessenger::emit(
                        messenger.handle(),
                        messenger.node_name(),
                        topic_name,
                        &#instance_id_ident,
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
                    #instance_id_stmt
                    let _ = #instance_id_ident;
                    let _ = messenger;
                    #(#ignore_params)*
                    Err(crate::Error::MessageFormatUnavailable {
                        context: String::from(#label_literal),
                    })
                }
            }
        }
    }
}

fn build_subscribed_topic_callback(
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
    let namespace_literal = Literal::string(topic.node.as_str());
    let reader_type = &encoding.reader_type;
    let schema_lookup = SchemaFieldLookup::new(artifacts.message_format());

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
            struct_prefix,
            &mut names,
        );
        field_statements.append(&mut statements);
        let field_ident = &param.ident;
        field_inits.push(quote!(#field_ident: #value_ident));
    }

    let context_literal = Literal::string(struct_prefix);

    quote! {
        pub async fn #fn_name(messenger: &crate::Messenger) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let namespace = #namespace_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let mut subscription = peppylib::TopicMessenger::listen(
                    messenger.handle(),
                    namespace,
                    topic_name,
                    qos,
                )
                .await
                .map_err(|source| crate::Error::TopicSubscribe {
                    topic_name: topic_name.to_string(),
                    namespace: namespace.to_string(),
                    source,
                })?;
                subscription
                    .on_next_message()
                    .await
                    .ok_or_else(|| crate::Error::SubscriptionClosed {
                        topic_name: topic_name.to_string(),
                    })?
            };

            let payload = message.payload().as_bytes();
            let instance_id = message.instance_id().unwrap_or("").to_string();
            let message = #helper_fn_ident(payload.as_ref())?;
            Ok((instance_id, message))
        }

        fn #helper_fn_ident(payload: &[u8]) -> crate::Result<#args_struct_ident> {
            let mut cursor = std::io::Cursor::new(payload);
            let message_reader = capnp::serialize::read_message(
                &mut cursor,
                capnp::message::ReaderOptions::new(),
            )
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

fn generate_field_reader_statements(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    generate_field_reader_statements_inner(
        reader_expr,
        field_name,
        schema,
        struct_prefix,
        context_label,
        names,
        true,
    )
}

fn generate_field_reader_statements_inner(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_label: &str,
    names: &mut NameGenerator,
    handle_optional: bool,
) -> (Vec<TokenStream>, Ident) {
    if handle_optional && schema.is_optional() {
        let option_ident = names.next(&format!("{field_name}_opt"));
        if schema_supports_presence_check(schema) {
            let has_method = Ident::new(
                &format!("has_{}", sanitize_component(field_name)),
                Span::call_site(),
            );
            let (inner_statements, value_ident) = generate_field_reader_statements_inner(
                reader_expr,
                field_name,
                schema,
                struct_prefix,
                context_label,
                names,
                false,
            );
            let statements = vec![quote! {
                let #option_ident = if #reader_expr.reborrow().#has_method() {
                    #( #inner_statements )*
                    Some(#value_ident)
                } else {
                    None
                };
            }];
            return (statements, option_ident);
        } else {
            let (mut statements, value_ident) = generate_field_reader_statements_inner(
                reader_expr,
                field_name,
                schema,
                struct_prefix,
                context_label,
                names,
                false,
            );
            statements.push(quote!(let #option_ident = Some(#value_ident);));
            return (statements, option_ident);
        }
    }

    match schema {
        SchemaType::Type(token) => {
            generate_primitive_reader(reader_expr, field_name, token, context_label, names)
        }
        SchemaType::Primitive(primitive) => generate_primitive_reader(
            reader_expr,
            field_name,
            &primitive.kind,
            context_label,
            names,
        ),
        SchemaType::Array(array) => {
            generate_array_reader(reader_expr, field_name, array, context_label, names)
        }
        SchemaType::Object(object) => generate_object_reader(
            reader_expr,
            field_name,
            &object.fields,
            struct_prefix,
            context_label,
            names,
        ),
    }
}

fn schema_supports_presence_check(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Type(token) => matches!(
            token,
            TypeToken::String | TypeToken::Bytes | TypeToken::Time
        ),
        SchemaType::Primitive(primitive) => matches!(
            primitive.kind,
            TypeToken::String | TypeToken::Bytes | TypeToken::Time
        ),
        SchemaType::Array(_) | SchemaType::Object(_) => true,
    }
}

fn generate_primitive_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    token: &TypeToken,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context_label);

    match token {
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => {
            let statement = quote! {
                let #value_ident = #reader_expr.reborrow().#method_ident();
            };
            (vec![statement], value_ident)
        }
        TypeToken::String => {
            let reader_ident = names.next("text");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: String::from(#context_literal),
                            source,
                        })?;
                },
                quote! {
                    let #value_ident = #reader_ident
                        .to_str()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: String::from(#context_literal),
                            source: capnp::Error::failed(source.to_string()),
                        })?
                        .to_owned();
                },
            ];
            (statements, value_ident)
        }
        TypeToken::Bytes => {
            let reader_ident = names.next("data");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: String::from(#context_literal),
                            source,
                        })?;
                },
                quote! {
                    let len = #reader_ident.len();
                    let mut #value_ident = Vec::with_capacity(len as usize);
                    for byte in #reader_ident.iter() {
                        #value_ident.push(*byte);
                    }
                },
            ];
            (statements, value_ident)
        }
        TypeToken::Time => {
            let reader_ident = names.next("timestamp");
            let capnp_ident = names.next("capnp_timestamp");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: String::from(#context_literal),
                            source,
                        })?;
                },
                quote! {
                    let #capnp_ident = peppylib::encoding::CapnpTimestamp {
                        sec: #reader_ident.get_sec(),
                        nsec: #reader_ident.get_nsec(),
                    };
                    let #value_ident = peppylib::encoding::convert_time_from_capnp(#capnp_ident);
                },
            ];
            (statements, value_ident)
        }
    }
}

fn generate_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    array: &ArraySchema,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    match array.items.as_ref().as_type_token() {
        Some(TypeToken::U8) => generate_u8_array_reader(
            reader_expr,
            field_name,
            &method_ident,
            array.length,
            context_label,
            names,
        ),
        Some(token) => generate_primitive_array_reader(
            reader_expr,
            field_name,
            token,
            &method_ident,
            array.length,
            context_label,
            names,
        ),
        None => panic!("unsupported nested schema type in array `{field_name}`"),
    }
}

fn generate_u8_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    method_ident: &Ident,
    length: Option<usize>,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("data");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context_label);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: String::from(#context_literal),
                source,
            })?;
    }];

    match length {
        Some(len) => {
            let len_lit = Literal::usize_unsuffixed(len);
            let array_ident = names.next("bytes");
            statements.push(quote! {
                if #reader_ident.len() as usize != #len_lit {
                    let actual = #reader_ident.len() as usize;
                    return Err(crate::Error::InvalidFixedBytes {
                        field: String::from(#field_literal),
                        expected: #len_lit,
                        actual,
                    });
                }
            });
            statements.push(quote! {
                let mut #array_ident = [0u8; #len_lit];
                for (idx, byte) in #reader_ident.iter().enumerate() {
                    #array_ident[idx] = *byte;
                }
                let #value_ident = #array_ident;
            });
        }
        None => {
            let vec_ident = names.next("bytes");
            statements.push(quote! {
                let mut #vec_ident = Vec::with_capacity(#reader_ident.len() as usize);
                for byte in #reader_ident.iter() {
                    #vec_ident.push(*byte);
                }
                let #value_ident = #vec_ident;
            });
        }
    }

    (statements, value_ident)
}

fn generate_primitive_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    token: &TypeToken,
    method_ident: &Ident,
    length: Option<usize>,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("list");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context_label);

    let element_ty = primitive_type_token(token);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: String::from(#context_literal),
                source,
            })?;
    }];

    if let Some(len) = length {
        let len_lit = Literal::usize_unsuffixed(len);
        statements.push(quote! {
            if #reader_ident.len() as usize != #len_lit {
                let actual = #reader_ident.len() as usize;
                return Err(crate::Error::InvalidFixedListLength {
                    field: String::from(#field_literal),
                    expected: #len_lit,
                    actual,
                });
            }
        });
        statements.push(quote! {
            let mut #value_ident: [#element_ty; #len_lit] = [#element_ty::default(); #len_lit];
            for (idx, element) in #reader_ident.iter().enumerate() {
                #value_ident[idx] = element;
            }
        });
    } else {
        let vec_ident = names.next("values");
        statements.push(quote! {
            let mut #vec_ident = Vec::with_capacity(#reader_ident.len() as usize);
            for value in #reader_ident.iter() {
                #vec_ident.push(value);
            }
            let #value_ident = #vec_ident;
        });
    }

    (statements, value_ident)
}

fn generate_object_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    object: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    context_label: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let reader_ident = names.next("reader");
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context_label);
    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: String::from(#context_literal),
                source,
            })?;
    }];

    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();
    let nested_prefix = format!("{struct_prefix}{}", to_camel_case(field_name));

    for (nested_name, nested_schema) in object {
        let (mut nested_statements, nested_value_ident) = generate_field_reader_statements(
            &quote!(#reader_ident),
            nested_name.as_str(),
            nested_schema,
            &nested_prefix,
            &nested_prefix,
            names,
        );
        field_statements.append(&mut nested_statements);
        let field_ident = Ident::new(&sanitize_component(nested_name), Span::call_site());
        field_inits.push(quote!(#field_ident: #nested_value_ident));
    }

    statements.extend(field_statements);

    let struct_name = format!("{struct_prefix}{}", to_camel_case(field_name));
    let struct_ident = Ident::new(&struct_name, Span::call_site());
    let value_ident = names.next(field_name);
    statements.push(quote! {
        let #value_ident = #struct_ident {
            #( #field_inits ),*
        };
    });

    (statements, value_ident)
}

fn qos_profile_tokens(profile: &QoSProfile) -> TokenStream {
    let variant = match profile {
        QoSProfile::Standard => "Standard",
        QoSProfile::Reliable => "Reliable",
        QoSProfile::SensorData => "SensorData",
        QoSProfile::Critical => "Critical",
    };
    let variant_ident = Ident::new(variant, Span::call_site());
    quote!(peppylib::config::QoSProfile::#variant_ident)
}

fn function_param_tokens(params: &[FunctionParam]) -> Vec<TokenStream> {
    params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect()
}

fn build_exposed_service_method(
    fn_name: &Ident,
    handler_fn_name_override: Option<&Ident>,
    handler_helper_name_override: Option<&Ident>,
    request_deserializer_name_override: Option<&Ident>,
    wire_params: &[FunctionParam],
    handler_params: &[FunctionParam],
    instance_id_param: Option<&FunctionParam>,
    encoding: Option<&MessageEncodingSpec>,
    request_format: Option<&MessageFormat>,
    label: &str,
    service_name_literal: &Literal,
    request_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
) -> (TokenStream, Vec<TokenStream>) {
    let handler_fn_name = handler_fn_name_override.cloned().unwrap_or_else(|| {
        Ident::new(
            &format!("handle_{}_next_request", fn_name),
            Span::call_site(),
        )
    });

    // Build callback parameter signature (just types, no names for Fn trait bounds)
    let mut callback_param_types: Vec<TokenStream> = Vec::new();
    if let Some(instance_param) = instance_id_param {
        callback_param_types.push(instance_param.ty.clone());
    }
    if let Some(request_struct) = request_struct {
        callback_param_types.push(quote!(#request_struct));
    } else {
        callback_param_types.extend(handler_params.iter().map(|p| p.ty.clone()));
    }

    // Determine response type
    let response_ty = response_spec
        .as_ref()
        .map(|spec| {
            let struct_ident = &spec.struct_ident;
            quote!(#struct_ident)
        })
        .unwrap_or_else(|| quote!(()));

    let service_name = service_name_literal;

    let needs_service_instance_id = response_spec
        .map(|spec| spec.include_service_instance_id)
        .unwrap_or(false);
    let service_instance_param_ident = if needs_service_instance_id {
        Some(Ident::new("service_instance_id", Span::call_site()))
    } else {
        None
    };
    let service_instance_call_arg = service_instance_param_ident
        .as_ref()
        .map(|_| quote!(service_instance_id.as_str()));

    let request_has_instance_id = request_format
        .map(|format| format.0.contains_key("instance_id"))
        .unwrap_or(false);
    let instance_from_request_context = instance_id_param.is_some() && !request_has_instance_id;

    let instance_binding_ident =
        instance_id_param.map(|_| Ident::new("instance_id", Span::call_site()));
    let (request_pattern, handler_request_args): (TokenStream, Vec<TokenStream>) =
        if request_struct.is_some() {
            let binding_ident = Ident::new("request_data", Span::call_site());
            (quote!(#binding_ident), vec![quote!(#binding_ident)])
        } else if handler_params.is_empty() {
            (quote!(()), Vec::new())
        } else if handler_params.len() == 1 {
            let ident = &handler_params[0].ident;
            (quote!((#ident,)), vec![quote!(#ident)])
        } else {
            let idents: Vec<&Ident> = handler_params.iter().map(|p| &p.ident).collect();
            let args = idents.iter().map(|ident| quote!(#ident)).collect();
            (quote!((#(#idents),*)), args)
        };
    let callback_call = if let Some(instance_ident) = instance_binding_ident.as_ref() {
        let mut handler_args = Vec::with_capacity(handler_request_args.len() + 1);
        handler_args.push(quote!(#instance_ident));
        handler_args.extend(handler_request_args.iter().cloned());
        quote!(handler(#(#handler_args),*))
    } else if handler_request_args.is_empty() {
        quote!(handler())
    } else if handler_request_args.len() == 1 {
        let arg = &handler_request_args[0];
        quote!(handler(#arg))
    } else {
        quote!(handler(#(#handler_request_args),*))
    };

    let response_serialization = build_response_serialization_code(
        response_spec,
        label,
        &callback_call,
        service_instance_param_ident.as_ref(),
    );
    let handler_helper_name = handler_helper_name_override.cloned().unwrap_or_else(|| {
        Ident::new(
            &format!("{}_handle_request_payload", fn_name),
            Span::call_site(),
        )
    });
    let request_deserializer_name =
        request_deserializer_name_override
            .cloned()
            .unwrap_or_else(|| {
                Ident::new(
                    &format!("{}_deserialize_request", fn_name),
                    Span::call_site(),
                )
            });

    let mut helper_tokens = Vec::new();

    if let Some(request_spec) = encoding {
        let request_format =
            request_format.expect("request format should exist when encoding is present");
        let request_deserializer = if instance_from_request_context {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                request_struct,
                None,
            )
        } else {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                request_struct,
                instance_id_param,
            )
        };
        helper_tokens.push(request_deserializer);

        let deserializer_pattern = if let Some(instance_ident) = instance_binding_ident.as_ref() {
            if instance_from_request_context {
                request_pattern.clone()
            } else {
                let request_pattern = request_pattern.clone();
                quote!((#instance_ident, #request_pattern))
            }
        } else {
            request_pattern.clone()
        };

        let mut helper_params: Vec<TokenStream> = vec![quote!(payload: &[u8]), quote!(handler: &F)];

        if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = quote! {
            fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                let #deserializer_pattern = #request_deserializer_name(payload)?;

                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        };
        helper_tokens.push(helper_fn);
    } else {
        let mut helper_params: Vec<TokenStream> = vec![quote!(handler: &F)];

        if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = quote! {
            fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        };
        helper_tokens.push(helper_fn);
    }

    let request_context_ident = if encoding.is_some() || instance_from_request_context {
        Ident::new("request_context", Span::call_site())
    } else {
        Ident::new("_request_context", Span::call_site())
    };

    let helper_call_tokens = if encoding.is_some() {
        let mut helper_args: Vec<TokenStream> = vec![quote!(payload.as_ref()), quote!(&handler)];

        if instance_from_request_context {
            helper_args.push(quote!(instance_id));
        }

        if let Some(arg) = service_instance_call_arg.clone() {
            helper_args.push(arg);
        }

        if instance_from_request_context {
            quote!({
                let payload = #request_context_ident.message().payload().as_bytes();
                let instance_id = #request_context_ident
                    .message()
                    .instance_id()
                    .map(str::to_string)
                    .ok_or_else(|| crate::Error::MissingInstanceId {
                        key_expr: #request_context_ident.message().key_expr().to_string(),
                    })?;
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            quote!({
                let payload = #request_context_ident.message().payload().as_bytes();
                #handler_helper_name(#(#helper_args),*)
            })
        }
    } else {
        let mut helper_args: Vec<TokenStream> = vec![quote!(&handler)];

        if instance_from_request_context {
            helper_args.push(quote!(instance_id));
        }

        if let Some(arg) = service_instance_call_arg.clone() {
            helper_args.push(arg);
        }

        if instance_from_request_context {
            quote!({
                let instance_id = #request_context_ident
                    .message()
                    .instance_id()
                    .map(str::to_string)
                    .ok_or_else(|| crate::Error::MissingInstanceId {
                        key_expr: #request_context_ident.message().key_expr().to_string(),
                    })?;
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            quote!(#handler_helper_name(#(#helper_args),*))
        }
    };

    let service_instance_env_stmt = {
        let env_var_literal = Literal::string("PEPPY_INSTANCE_ID");
        quote! {
            let service_instance_id = std::env::var(#env_var_literal).map_err(|source| {
                crate::Error::MissingInstanceIdEnvVar {
                    var: #env_var_literal,
                    source,
                }
            })?;
        }
    };
    let service_instance_clone_stmt = if needs_service_instance_id {
        quote!(let service_instance_id = service_instance_id.clone();)
    } else {
        TokenStream::new()
    };

    let method = quote! {
        pub async fn #handler_fn_name<F>(
            messenger: &crate::Messenger,
            handler: F,
        ) -> crate::Result<()>
        where
            F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
        {
            #service_instance_env_stmt
            let namespace = messenger.node_name();
            let service_name = #service_name;

            let mut service = peppylib::ServiceMessenger::listen(
                messenger.handle(),
                namespace,
                service_name,
                service_instance_id.as_str(),
            )
            .await?;

            service
                .handle_next_request(move |#request_context_ident| {
                    #service_instance_clone_stmt
                    async move {
                        #helper_call_tokens.map_err(|error| {
                            peppylib::PeppyError::Io(std::io::Error::other(error.to_string()))
                        })
                    }
                })
                .await?;

            Ok(())
        }
    };

    (method, helper_tokens)
}

fn build_request_struct_with_name(
    struct_name: &str,
    params: &[FunctionParam],
) -> Option<(Ident, TokenStream)> {
    if params.is_empty() {
        return None;
    }

    let ident = Ident::new(struct_name, Span::call_site());
    let field_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(pub #ident: #ty)
        })
        .collect();
    let ctor_params: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();
    let ctor_bindings: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            quote!(#ident)
        })
        .collect();

    let tokens = quote! {
        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub struct #ident {
            #( #field_tokens ),*
        }

        impl #ident {
            pub fn new(#(#ctor_params),*) -> Self {
                Self {
                    #( #ctor_bindings ),*
                }
            }
        }
    };

    Some((ident, tokens))
}

fn build_request_struct(
    struct_prefix: &str,
    params: &[FunctionParam],
) -> Option<(Ident, TokenStream)> {
    let struct_name = format!("{struct_prefix}Request");
    build_request_struct_with_name(&struct_name, params)
}

fn build_request_deserializer(
    deserializer_fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    request_format: &MessageFormat,
    wire_params: &[FunctionParam],
    handler_params: &[FunctionParam],
    label: &str,
    request_struct: Option<&Ident>,
    instance_id_param: Option<&FunctionParam>,
) -> TokenStream {
    if let Some(instance_param) = instance_id_param {
        let reader_type = &request_spec.reader_type;
        let context_literal = Literal::string(label);
        let instance_ty = &instance_param.ty;

        // Build return type for request data
        let request_return_ty = if let Some(request_struct) = request_struct {
            quote!(#request_struct)
        } else if handler_params.is_empty() {
            quote!(())
        } else if handler_params.len() == 1 {
            let ty = &handler_params[0].ty;
            quote!((#ty,))
        } else {
            let types: Vec<&TokenStream> = handler_params.iter().map(|p| &p.ty).collect();
            quote!((#(#types),*))
        };
        let return_ty = quote!((#instance_ty, #request_return_ty));

        // Generate field deserialization using schema metadata
        let schema_lookup = SchemaFieldLookup::new(request_format);

        let mut names = NameGenerator::new();
        let mut field_statements = Vec::new();
        let mut handler_value_map: HashMap<String, Ident> = HashMap::new();
        let mut instance_value_ident = None;
        let instance_field_key = instance_param.ident.to_string();

        for param in wire_params {
            let field_key = param.ident.to_string();
            let (original_name, schema) = schema_lookup.get(&field_key);

            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                original_name.as_str(),
                schema,
                label,
                label,
                &mut names,
            );
            field_statements.append(&mut statements);

            if field_key == instance_field_key {
                instance_value_ident = Some(value_ident);
            } else {
                handler_value_map.insert(field_key, value_ident);
            }
        }

        let instance_value_ident =
            instance_value_ident.expect("instance_id should be present in service requests");
        let mut ordered_request_values: Vec<Ident> = handler_params
            .iter()
            .map(|param| {
                let key = param.ident.to_string();
                handler_value_map
                    .get(&key)
                    .unwrap_or_else(|| panic!("missing field `{key}` in request payload"))
                    .clone()
            })
            .collect();

        let request_expr = if let Some(request_struct) = request_struct {
            let field_assignments: Vec<TokenStream> = handler_params
                .iter()
                .zip(ordered_request_values.iter())
                .map(|(param, value_ident)| {
                    let field_ident = &param.ident;
                    quote!(#field_ident: #value_ident)
                })
                .collect();
            quote!(#request_struct { #( #field_assignments ),* })
        } else if ordered_request_values.is_empty() {
            quote!(())
        } else if ordered_request_values.len() == 1 {
            let ident = ordered_request_values
                .pop()
                .expect("request value missing despite length check");
            quote!((#ident,))
        } else {
            quote!((#(#ordered_request_values),*))
        };

        quote! {
            fn #deserializer_fn_name(payload: &[u8]) -> crate::Result<#return_ty> {
                let mut cursor = std::io::Cursor::new(payload);
                let message_reader = capnp::serialize::read_message(
                        &mut cursor,
                        capnp::message::ReaderOptions::new(),
                    )
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

                Ok((#instance_value_ident, #request_expr))
            }
        }
    } else {
        let reader_type = &request_spec.reader_type;
        let context_literal = Literal::string(label);

        let return_ty = if let Some(request_struct) = request_struct {
            quote!(#request_struct)
        } else if handler_params.is_empty() {
            quote!(())
        } else if handler_params.len() == 1 {
            let ty = &handler_params[0].ty;
            quote!((#ty,))
        } else {
            let types: Vec<&TokenStream> = handler_params.iter().map(|p| &p.ty).collect();
            quote!((#(#types),*))
        };

        let schema_lookup = SchemaFieldLookup::new(request_format);

        let mut names = NameGenerator::new();
        let mut field_statements = Vec::new();
        let mut value_idents = Vec::new();

        for param in wire_params {
            let field_key = param.ident.to_string();
            let (original_name, schema) = schema_lookup.get(&field_key);

            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                original_name.as_str(),
                schema,
                label,
                label,
                &mut names,
            );
            field_statements.append(&mut statements);
            value_idents.push(value_ident);
        }

        let request_expr = if let Some(request_struct) = request_struct {
            let field_assignments: Vec<TokenStream> = handler_params
                .iter()
                .zip(value_idents.iter())
                .map(|(param, value_ident)| {
                    let field_ident = &param.ident;
                    quote!(#field_ident: #value_ident)
                })
                .collect();
            quote!(#request_struct { #( #field_assignments ),* })
        } else if value_idents.is_empty() {
            quote!(())
        } else if value_idents.len() == 1 {
            let ident = &value_idents[0];
            quote!((#ident,))
        } else {
            quote!((#(#value_idents),*))
        };

        quote! {
            fn #deserializer_fn_name(payload: &[u8]) -> crate::Result<#return_ty> {
                let mut cursor = std::io::Cursor::new(payload);
                let message_reader = capnp::serialize::read_message(
                        &mut cursor,
                        capnp::message::ReaderOptions::new(),
                    )
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

                Ok(#request_expr)
            }
        }
    }
}
fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    label: &str,
    callback_call: &TokenStream,
    service_instance_ident: Option<&Ident>,
) -> TokenStream {
    let Some(spec) = response_spec else {
        return quote!({
            #callback_call?;
            bytes::Bytes::new()
        });
    };

    let builder_type = &spec.builder_type;
    let format = spec.format;
    let context_literal = Literal::string(label);
    let builder_ident = Ident::new("response_root", Span::call_site());
    let mut assignments = Vec::new();
    let mut names = NameGenerator::new();
    let response_ident = Ident::new("response", Span::call_site());
    let include_service_instance_id = spec.include_service_instance_id;

    for (field_name, schema) in &format.0 {
        if include_service_instance_id && field_name == "instance_id" {
            let instance_ident = service_instance_ident
                .expect("service instance identifier should be available when required");
            assignments.push(quote!(#builder_ident.set_instance_id(#instance_ident);));
            continue;
        }

        let field_ident = Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
        let value_expr = quote!(#response_ident.#field_ident);
        assignments.push(generate_field_assignment(
            &quote!(#builder_ident),
            field_name,
            schema,
            &value_expr,
            &mut names,
        ));
    }

    quote!({
        let response = #callback_call?;

        let mut message = capnp::message::Builder::new_default();
        {
            let mut #builder_ident = message.init_root::<#builder_type>();
            #( #assignments )*
        }
        let mut buffer = Vec::new();
        capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
            crate::Error::CapnpSerialize {
                context: String::from(#context_literal),
                source,
            }
        })?;
        bytes::Bytes::from(buffer)
    })
}

fn to_camel_case(raw: &str) -> String {
    let sanitized = sanitize_component(raw);
    let mut out = String::new();

    for segment in sanitized.split('_').filter(|segment| !segment.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }

    if out.is_empty() {
        out.push_str("Item");
    }

    if !out
        .chars()
        .next()
        .map(|ch| ch.is_ascii_alphabetic())
        .unwrap_or(true)
    {
        out.insert(0, 'T');
    }

    out
}
