#[cfg(test)]
mod tests;

mod actions;
mod context;
mod deserialization;
mod identifiers;
mod parameters;
mod scaffold;
mod serialization;
mod services;
mod topics;
mod type_mapping;

pub use parameters::{generate_parameters_struct, validate_parameter_schema};

use super::types::{
    CapnpSchema, ConsumedActionMessage, DependencyContext, InterfaceArtifact, InterfaceKind,
    InterfaceOrigin, LanguageGenerator, goal_action_response_format, non_empty_message_format,
    scoped_schema_key,
};
use crate::error::Result;
use crate::generator::naming::{
    module_name_from_components, non_empty_str, raw_module_label, resolve_schema_file_stem,
    sanitize_component, sanitize_node_display_name, to_camel_case,
};
use config::encoding::{CapnpSchemaArtifacts, FunctionParam};
use config::node::{
    ConsumedAction, ConsumedService, ConsumedTopic, EmittedTopic, ExposedAction, ExposedService,
    MessageFormat,
};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;
use std::path::Path;

use actions::{
    build_action_expose_method, build_action_handle_struct, build_action_request_deserializer,
    build_goal_context_base_methods, build_goal_context_complete,
    build_goal_context_publish_feedback, build_goal_context_struct,
    build_goal_response_constructors, build_handle_goal_next_request,
};
use context::{GenerationContext, collect_function_params, map_message_format};
use deserialization::{build_deserialize_fn, deserialize_format_fields};
use serialization::{
    MessageEncodingSpec, build_serialize_payload, generate_assignments_for_format,
    generate_assignments_from_struct,
};
use services::{
    ExposedServiceMethodSpec, ServiceResponseSpec, build_exposed_service_method,
    build_request_struct_with_name_and_impl, build_response_payload_tokens,
    deserialize_fields_from_format,
};
use topics::{
    ConsumedTopicCallbackSpec, build_consumed_topic_callback, build_topic_publisher,
    consumed_to_target_expression,
};
use type_mapping::{render_tokens, unused_params_stmt};

/// Rust-specific implementation of the interface generator.
#[derive(Default)]
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
    parameters: config::ParameterSchema,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    fn make_artifact(
        &self,
        leaf_name: &str,
        origin: Option<&InterfaceOrigin>,
        kind: InterfaceKind,
        code_output: String,
    ) -> InterfaceArtifact {
        InterfaceArtifact::for_leaf(origin, leaf_name, kind, code_output)
    }

    /// Sets the node parameters for code generation.
    pub fn set_parameters(&mut self, parameters: config::ParameterSchema) {
        self.parameters = parameters;
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }

    fn register_schema(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: &CapnpSchemaArtifacts,
    ) -> Result<SchemaInfo> {
        let resolved = resolve_schema_file_stem(schema_key);

        // When multiple callers (e.g. two consumed topics sharing producer+topic
        // with different link_ids) resolve to the same capnp file, the schema
        // file is written once but every caller still needs a Rust-side
        // reference to a struct that actually exists in it. Reuse the first
        // registration's struct identity for every subsequent collision.
        if let Some(existing) = self.schemas.get(&resolved.file_stem) {
            return Ok(SchemaInfo {
                file_stem: resolved.file_stem,
                struct_module: existing.struct_module().to_string(),
            });
        }

        let schema_source = artifacts.encoding_schema();
        let struct_name = format!("{struct_prefix}Message");
        let struct_module = crate::generator::naming::normalize_snake_case(&struct_name);
        let schema = schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let capnp_schema =
            CapnpSchema::new(resolved.file_stem.clone(), struct_module.clone(), schema);
        self.schemas
            .insert(resolved.file_stem.clone(), capnp_schema);

        Ok(SchemaInfo {
            file_stem: resolved.file_stem,
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
            generate_assignments_for_format(&builder_ident, artifacts.message_format(), params)?;

        Ok(Some(MessageEncodingSpec {
            builder_type: schema_info.builder_type_tokens(),
            assignments,
            reader_type: schema_info.reader_type_tokens(),
        }))
    }

    /// Builds the fire_goal method for consumed actions using ActionMessenger::send_goal
    fn build_consumed_action_fire_goal_method(
        &mut self,
        context: &mut GenerationContext,
        action_struct_name: &str,
        request_format: Option<&MessageFormat>,
        response_format: Option<&MessageFormat>,
        schema_key: &str,
        dependency: &DependencyContext,
    ) -> Result<(TokenStream, Vec<TokenStream>, bool)> {
        let request_artifacts =
            map_message_format(&format!("{schema_key}_request"), request_format)?;
        let response_artifacts =
            map_message_format(&format!("{schema_key}_response"), response_format)?;

        // Build GoalRequest struct if there's request data
        let goal_request_ident = Ident::new("GoalRequest", Span::call_site());
        if let Some(ref artifacts) = request_artifacts {
            let params = collect_function_params(
                Some(artifacts),
                None,
                &format!("{action_struct_name}Goal"),
                context,
                None,
            )?;
            let fields: Vec<(Ident, TokenStream)> = params
                .iter()
                .map(|p| (p.ident.clone(), p.ty.clone()))
                .collect();
            context.add_struct(goal_request_ident.clone(), fields);
        }

        // Build GoalResponseData struct
        let goal_response_data_ident = Ident::new("GoalResponseData", Span::call_site());
        let has_goal_response_data = response_artifacts.is_some();

        // Build goal payload encoding (shared between both branches)
        let goal_payload_tokens = if let Some(ref request_artifacts) = request_artifacts {
            let schema_info = self.register_schema(
                schema_key,
                &format!("{action_struct_name}GoalMessage"),
                request_artifacts,
            )?;
            let builder_type = schema_info.builder_type_tokens();

            let request_ident = Ident::new("request", Span::call_site());
            let assignments = generate_assignments_from_struct(
                &Ident::new("root", Span::call_site()),
                request_format.expect("request_format should be Some when artifacts exist"),
                &request_ident,
            )?;

            let error_context = quote!(format!(
                "fire_goal {} {}",
                TARGET_NODE_NAME, TARGET_ACTION_NAME
            ));
            let serialize_block =
                build_serialize_payload(&builder_type, &[], &assignments, &error_context);
            quote! { let goal_payload = #serialize_block; }
        } else {
            quote! {
                let goal_payload = peppylib::Payload::new();
            }
        };

        let request_param = if request_format.is_some() {
            quote!(request: #goal_request_ident,)
        } else {
            quote!()
        };

        // Build response-specific parts (struct definition, deserializer, result expression)
        let mut helper_items = Vec::new();
        let response_handling = if let Some(ref response_artifacts) = response_artifacts {
            let _response_params = collect_function_params(
                None,
                Some(response_artifacts),
                &format!("{action_struct_name}GoalResponse"),
                context,
                Some(&goal_response_data_ident),
            )?;

            // Build deserializer helper
            let response_schema_key = format!("{schema_key}_response");
            let response_schema = self.register_schema(
                &response_schema_key,
                "GoalResponseMessage",
                response_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();

            let response_format = response_artifacts.message_format();
            let context_expr = quote!(format!(
                "{} {} GoalResponse",
                TARGET_NODE_NAME, TARGET_ACTION_NAME
            ));
            let (response_statements, response_inits, _) =
                deserialize_format_fields(response_format, "GoalResponseData", &context_expr)?;

            let deserialize_helper = build_deserialize_fn(
                &Ident::new("deserialize_goal_response", Span::call_site()),
                &reader_type,
                &context_expr,
                &quote!(#goal_response_data_ident),
                &response_statements,
                &quote!(#goal_response_data_ident { #( #response_inits ),* }),
            );
            helper_items.push(deserialize_helper);

            quote! {
                let payload = action_handle.goal_response().payload();
                let response_data = deserialize_goal_response(payload.as_ref())?;
                Ok(Self {
                    messenger: node_runner.messenger().clone(),
                    inner: action_handle,
                    data: response_data,
                })
            }
        } else {
            quote! {
                Ok(Self {
                    messenger: node_runner.messenger().clone(),
                    inner: action_handle,
                })
            }
        };

        // The `to_target` matches the producer's emission shape: address the
        // dependency as an Interface if it exposes the action via
        // `conforms_to`, otherwise as its native Node identity. The `target`
        // — the producer's full `(core_node, instance_id)` — is resolved at
        // runtime from the consumer's binding map; pinned slots address it
        // directly and skip discovery.
        let to_target_expr = consumed_to_target_expression(dependency);
        let target_expr = crate::generator::rust::topics::consumed_target_expression(dependency);
        let method_tokens = quote! {
            pub async fn fire_goal(
                node_runner: &crate::NodeRunner,
                timeout: std::time::Duration,
                #request_param
                feedback_qos: peppylib::config::QoSProfile,
            ) -> crate::Result<Self> {
                #goal_payload_tokens

                let action_handle = peppylib::ActionMessenger::send_goal(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    #to_target_expr,
                    TARGET_ACTION_NAME,
                    #target_expr,
                    goal_payload,
                    feedback_qos,
                    timeout,
                )
                .await?;

                #response_handling
            }
        };

        Ok((method_tokens, helper_items, has_goal_response_data))
    }

    /// Builds the consumed action `get_result` method and its supporting types.
    ///
    /// `get_result` returns a typed [`ResultResponse`] carrying a `ResultOutcome`
    /// enum (Completed/Cancelled with the decoded result data, or the empty
    /// Abandoned/Expired terminal states). The engine frames the reply as a
    /// [`peppylib::messaging::ResultStatus`] tag plus the raw body, stripped by
    /// `ActionMessenger::request_result` into a typed reply, so this only needs
    /// to decode the body for the data-bearing variants.
    fn build_consumed_action_get_result_method(
        &mut self,
        context: &mut GenerationContext,
        action_struct_name: &str,
        result_artifacts: Option<CapnpSchemaArtifacts>,
        result_schema_key: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let data_ident = Ident::new("ResultResponseData", Span::call_site());
        let response_ident = Ident::new("ResultResponse", Span::call_site());
        let outcome_ident = Ident::new("ResultOutcome", Span::call_site());
        let mut helper_items = Vec::new();

        let response_struct = quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            pub struct #response_ident {
                pub instance_id: String,
                pub core_node: String,
                pub outcome: #outcome_ident,
            }
        };

        let method_tokens = if let Some(result_artifacts) = result_artifacts {
            // Defines `ResultResponseData` (+ ctor) in the context.
            let _params = collect_function_params(
                None,
                Some(&result_artifacts),
                &format!("{action_struct_name}ResultResponse"),
                context,
                Some(&data_ident),
            )?;

            let response_schema = self.register_schema(
                result_schema_key,
                "ResultResponseMessage",
                &result_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();
            let response_format = result_artifacts.message_format();
            let context_expr = quote!(format!(
                "{} {} {}",
                TARGET_NODE_NAME, TARGET_ACTION_NAME, "ResultResponse"
            ));
            let (statements, inits, _) =
                deserialize_format_fields(response_format, "ResultResponseData", &context_expr)?;
            helper_items.push(build_deserialize_fn(
                &Ident::new("deserialize_result_response", Span::call_site()),
                &reader_type,
                &context_expr,
                &quote!(#data_ident),
                &statements,
                &quote!(#data_ident { #( #inits ),* }),
            ));

            helper_items.push(quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub enum #outcome_ident {
                    Completed(#data_ident),
                    Cancelled(#data_ident),
                    Abandoned,
                    Expired,
                }
            });
            helper_items.push(response_struct);

            quote! {
                pub async fn get_result(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<#response_ident> {
                    let reply = peppylib::ActionMessenger::request_result(
                        &self.messenger,
                        &self.inner,
                        timeout,
                    )
                    .await?;
                    let outcome = match reply.status {
                        peppylib::messaging::ResultStatus::Completed => {
                            #outcome_ident::Completed(deserialize_result_response(reply.body.as_ref())?)
                        }
                        peppylib::messaging::ResultStatus::Cancelled => {
                            #outcome_ident::Cancelled(deserialize_result_response(reply.body.as_ref())?)
                        }
                        peppylib::messaging::ResultStatus::Abandoned => #outcome_ident::Abandoned,
                        peppylib::messaging::ResultStatus::Expired => #outcome_ident::Expired,
                    };
                    Ok(#response_ident {
                        instance_id: reply.instance_id,
                        core_node: reply.core_node,
                        outcome,
                    })
                }
            }
        } else {
            helper_items.push(quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub enum #outcome_ident {
                    Completed,
                    Cancelled,
                    Abandoned,
                    Expired,
                }
            });
            helper_items.push(response_struct);

            quote! {
                pub async fn get_result(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<#response_ident> {
                    let reply = peppylib::ActionMessenger::request_result(
                        &self.messenger,
                        &self.inner,
                        timeout,
                    )
                    .await?;
                    let outcome = match reply.status {
                        peppylib::messaging::ResultStatus::Completed => #outcome_ident::Completed,
                        peppylib::messaging::ResultStatus::Cancelled => #outcome_ident::Cancelled,
                        peppylib::messaging::ResultStatus::Abandoned => #outcome_ident::Abandoned,
                        peppylib::messaging::ResultStatus::Expired => #outcome_ident::Expired,
                    };
                    Ok(#response_ident {
                        instance_id: reply.instance_id,
                        core_node: reply.core_node,
                        outcome,
                    })
                }
            }
        };

        Ok((method_tokens, helper_items))
    }

    /// Builds the consumed action `cancel_goal` method, its `CancelResponse`
    /// type, and a per-action `CancelState` enum. The cancel reply is the
    /// framework-owned cancel-ack, decoded via `peppylib::messaging::decode_cancel_ack`
    /// (there is no per-action cancel payload schema); the engine's `CancelState`
    /// is then mapped onto a generated enum so user code never has to name a
    /// `peppylib` type (the user crate only depends on the generated `peppygen`).
    fn build_consumed_action_cancel_method(
        &mut self,
        context: &mut GenerationContext,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let response_ident = Ident::new("CancelResponse", Span::call_site());
        let state_ident = Ident::new("CancelState", Span::call_site());
        context.add_struct(
            response_ident.clone(),
            vec![
                (Ident::new("instance_id", Span::call_site()), quote!(String)),
                (Ident::new("core_node", Span::call_site()), quote!(String)),
                (Ident::new("state", Span::call_site()), quote!(#state_ident)),
            ],
        );

        let helper_items = vec![quote! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            #[allow(dead_code)]
            pub enum #state_ident {
                Signalled,
                AlreadyTerminal,
                Unknown,
            }
        }];

        let method_tokens = quote! {
            pub async fn cancel_goal(
                &self,
                timeout: std::time::Duration,
            ) -> crate::Result<#response_ident> {
                let response = peppylib::ActionMessenger::cancel_goal(
                    &self.messenger,
                    &self.inner,
                    timeout,
                )
                .await?;
                let state = match peppylib::messaging::decode_cancel_ack(response.payload().as_ref())? {
                    peppylib::messaging::CancelState::Signalled => #state_ident::Signalled,
                    peppylib::messaging::CancelState::AlreadyTerminal => #state_ident::AlreadyTerminal,
                    peppylib::messaging::CancelState::Unknown => #state_ident::Unknown,
                };
                Ok(#response_ident {
                    instance_id: response.instance_id().to_string(),
                    core_node: response.core_node().to_string(),
                    state,
                })
            }
        };

        Ok((method_tokens, helper_items))
    }

    /// Builds the on_next_feedback_message method for consumed actions.
    ///
    /// `schema_key` is the producer-scoped key from
    /// [`consumed_action_schema_keys`][`crate::generator::naming::consumed_action_schema_keys`]
    /// and determines both the artifact name (drives the cap'n proto schema_id)
    /// and the registered file_stem.
    fn build_consumed_action_feedback_method(
        &mut self,
        context: &mut GenerationContext,
        format: &MessageFormat,
        action_struct_name: &str,
        schema_key: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let format_artifacts = map_message_format(schema_key, Some(format))?
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

        let encoding = self
            .prepare_message_encoding(schema_key, &struct_prefix, Some(&format_artifacts), &params)?
            .expect("feedback encoding spec should exist");
        let reader_type = encoding.reader_type.clone();

        let helper_fn_ident = Ident::new("deserialize_feedback_payload", Span::call_site());

        let format_schema = format_artifacts.message_format();
        let feedback_context_expr = quote!(format!(
            "{} {} FeedbackMessage",
            TARGET_NODE_NAME, TARGET_ACTION_NAME
        ));
        let (field_statements, value_idents) = deserialize_fields_from_format(
            format_schema,
            &params,
            &schema_struct_name,
            &feedback_context_expr,
        )?;
        let field_inits: Vec<TokenStream> = params
            .iter()
            .zip(value_idents.iter())
            .map(|(param, value_ident)| {
                let field_ident = &param.ident;
                quote!(#field_ident: #value_ident)
            })
            .collect();
        let helper_tokens = build_deserialize_fn(
            &helper_fn_ident,
            &reader_type,
            &feedback_context_expr,
            &quote!(#struct_ident),
            &field_statements,
            &quote!(#struct_ident { #( #field_inits ),* }),
        );

        let method_ident = Ident::new("on_next_feedback_message", Span::call_site());
        let method_tokens = quote! {
            /// Receives the next feedback message for this goal.
            ///
            /// Terminal errors that end a feedback drain loop:
            /// `Error::ActionFeedbackChannelClosed` when the producer closed
            /// the stream cleanly (end-of-stream sentinel), and
            /// `Error::ActionFeedbackProducerGone` when the producer instance
            /// disappeared without closing it (process killed, crashed); in
            /// the latter case `get_result` resolves to
            /// `ResultOutcome::Abandoned`.
            pub async fn #method_ident(
                &mut self,
            ) -> crate::Result<#struct_ident> {
                let feedback = self.inner.on_next_feedback().await?;
                let payload = feedback.payload();
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

impl LanguageGenerator for RustGenerator {
    fn add_emitted_topic(
        &mut self,
        topic: &EmittedTopic,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();

        let schema_prefix = to_camel_case(&fn_name_str);
        let struct_prefix = String::from("Message");

        let mut context = GenerationContext::default();
        let format_artifacts = map_message_format(&fn_name_str, topic.message_format.as_ref())?;
        let params = collect_function_params(
            format_artifacts.as_ref(),
            None,
            &struct_prefix,
            &mut context,
            None,
        )?;
        let scoped_key = scoped_schema_key(origin, &fn_name_str);
        let encoding = self.prepare_message_encoding(
            &scoped_key,
            &schema_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();
        let method_tokens =
            build_topic_publisher(&params, encoding.as_ref(), topic, &fn_name_str, origin);

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*
            #method_tokens
        };
        let rendered = render_tokens(tokens);

        let mut module_label = topic.name.trim().to_string();
        if module_label.is_empty() {
            module_label = String::from("topic");
        }

        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&module_label),
            origin,
            InterfaceKind::EmittedTopic,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_service(
        &mut self,
        service: &ExposedService,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let fn_name_str = fn_name.to_string();
        let struct_prefix = to_camel_case(&fn_name_str);
        let generic_response_ident = Ident::new("Response", Span::call_site());
        let generic_handler_ident = Ident::new("handle_next_request", Span::call_site());
        let generic_helper_ident = Ident::new("handle_request_payload", Span::call_site());
        let generic_deserializer_ident = Ident::new("deserialize_request", Span::call_site());

        let mut context = GenerationContext::default();
        let request_format = non_empty_message_format(service.request_message_format.as_ref());
        let request_wire_artifacts =
            map_message_format(&format!("{fn_name_str}_request"), request_format)?;
        let response_artifacts = map_message_format(
            &format!("{fn_name_str}_response"),
            service.response_message_format.as_ref(),
        )?;
        let wire_params = collect_function_params(
            request_wire_artifacts.as_ref(),
            response_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
            Some(&generic_response_ident),
        )?;
        let scoped_request_key = scoped_schema_key(origin, &fn_name_str);
        let encoding = self.prepare_message_encoding(
            &scoped_request_key,
            &struct_prefix,
            request_wire_artifacts.as_ref(),
            &wire_params,
        )?;

        let handler_params = wire_params.clone();
        let instance_id_param =
            FunctionParam::new(Ident::new("instance_id", Span::call_site()), quote!(String));

        let request_data_struct_ident = if let Some((ident, tokens)) =
            build_request_struct_with_name_and_impl("RequestData", &handler_params, false)
        {
            context.add_private_struct(tokens);
            Some(ident)
        } else {
            None
        };

        let request_struct_ident = Ident::new("Request", Span::call_site());
        context.add_metadata_struct(
            request_struct_ident.clone(),
            request_data_struct_ident.as_ref(),
        );
        let request_struct_ident = Some(request_struct_ident);

        let response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
            let response_prefix = format!("{struct_prefix}Response");
            let schema_key = scoped_schema_key(origin, &format!("{fn_name_str}_response"));
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
        let (method_token, helper_tokens) =
            build_exposed_service_method(&ExposedServiceMethodSpec {
                fn_name: &fn_name,
                handler_fn_name_override: Some(&generic_handler_ident),
                handler_helper_name_override: Some(&generic_helper_ident),
                request_deserializer_name_override: Some(&generic_deserializer_ident),
                wire_params: &wire_params,
                handler_params: &handler_params,
                instance_id_param: Some(&instance_id_param),
                encoding: encoding.as_ref(),
                request_format,
                label: &fn_name_str,
                service_name_literal: &service_name_literal,
                request_struct: request_struct_ident.as_ref(),
                request_data_struct: request_data_struct_ident.as_ref(),
                response_spec: response_spec.as_ref(),
                use_service_name_const: true,
                origin,
            })?;

        let service_name_const = {
            let service_name_str = Literal::string(service.name.as_str());
            quote!(const SERVICE_NAME: &str = #service_name_str;)
        };

        let mut service_tokens = vec![service_name_const];
        service_tokens.extend(context.into_tokens());
        service_tokens.push(method_token);
        service_tokens.extend(helper_tokens);

        let mut module_label = service.name.trim().to_string();
        if sanitize_component(&module_label).is_empty() {
            module_label = fn_name_str.clone();
        }

        let tokens: TokenStream = quote! {
            #( #service_tokens )*
        };
        let rendered = render_tokens(tokens);
        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&module_label),
            origin,
            InterfaceKind::ExposedService,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_action(
        &mut self,
        action: &ExposedAction,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let base_ident = prefixed_ident("", non_empty_str(&action.name), "action");
        let base_name = base_ident.to_string();
        let action_prefix = to_camel_case(&base_name);

        let mut context = GenerationContext::default();
        // Methods on `impl ActionHandle` (expose + handle_goal_next_request).
        let mut action_handle_methods: Vec<TokenStream> = Vec::new();
        // Methods on `impl GoalContext` (request/goal_id/cancel_signal/... plus
        // publish_feedback and complete/complete_cancelled when applicable).
        let mut goal_context_methods: Vec<TokenStream> = Vec::new();
        let mut helper_tokens: Vec<TokenStream> = Vec::new();
        // Free items (the GoalResponse constructors).
        let mut extra_items: Vec<TokenStream> = Vec::new();

        let action_name_literal = Literal::string(&action.name);

        let has_feedback = action.feedback_topic.is_some();

        // Every action is goal-driven in the concurrent model: clients fire a
        // goal, the server accepts/rejects and (when accepted) drives it through
        // a GoalContext. An absent `goal_service` simply means the goal carries
        // no request/response payload.
        {
            let goal_service = action.goal_service.as_ref();
            let label = format!("{base_name}_goal");
            let schema_struct_prefix = format!("{action_prefix}Goal");

            let request_artifacts = map_message_format(
                &format!("{label}_request"),
                goal_service.and_then(|goal| goal.request_message_format.as_ref()),
            )?;
            // The goal acknowledgement is framework-owned ({accepted,
            // error_message}); any goal response declared in the action schema
            // is ignored. The decider returns GoalResponse::accept() /
            // GoalResponse::reject(reason).
            let goal_response_format = goal_action_response_format();
            let response_artifacts =
                map_message_format(&format!("{label}_response"), Some(&goal_response_format))?;

            // Generates `GoalResponse` (+ `new`) when there is a response, and
            // returns the request params used to build `GoalRequestData`.
            let goal_data_params = collect_function_params(
                request_artifacts.as_ref(),
                response_artifacts.as_ref(),
                "Goal",
                &mut context,
                None,
            )?;

            let goal_request_data_struct = if let Some((ident, tokens)) =
                build_request_struct_with_name_and_impl("GoalRequestData", &goal_data_params, false)
            {
                context.add_private_struct(tokens);
                Some(ident)
            } else {
                None
            };

            context.add_metadata_struct(
                Ident::new("GoalRequest", Span::call_site()),
                goal_request_data_struct.as_ref(),
            );

            let scoped_goal_key = scoped_schema_key(origin, &label);
            let encoding = self.prepare_message_encoding(
                &scoped_goal_key,
                &schema_struct_prefix,
                request_artifacts.as_ref(),
                &goal_data_params,
            )?;

            // A deserializer is only needed when the goal carries request data
            // (an empty `{}` request format yields an encoding but no
            // `GoalRequestData` struct, so there is nothing to decode).
            if let (Some(_), Some(spec)) = (goal_request_data_struct.as_ref(), encoding.as_ref()) {
                let deserializer_fn = build_action_request_deserializer(
                    &Ident::new("deserialize_goal_request", Span::call_site()),
                    spec,
                    request_artifacts
                        .as_ref()
                        .map(|a| a.message_format())
                        .unwrap_or(&MessageFormat(IndexMap::new())),
                    &goal_data_params,
                    &label,
                    goal_request_data_struct.as_ref(),
                )?;
                helper_tokens.push(deserializer_fn);
            }

            // The decider returns a framework `GoalResponse`
            // (`accept()` / `reject(reason)`).
            extra_items.push(build_goal_response_constructors());

            // Serialization for the `GoalResponse` (reads a local `response`).
            // The goal response is framework-owned, so it is always present.
            let return_artifacts = response_artifacts
                .as_ref()
                .expect("framework goal response format always yields artifacts");
            let response_schema_prefix = format!("{schema_struct_prefix}Response");
            let schema_key = scoped_schema_key(origin, &format!("{label}_response"));
            let schema_info =
                self.register_schema(&schema_key, &response_schema_prefix, return_artifacts)?;
            let spec = ServiceResponseSpec {
                format: return_artifacts.message_format(),
                struct_ident: Ident::new("GoalResponse", Span::call_site()),
                builder_type: schema_info.builder_type_tokens(),
                include_service_instance_id: false,
            };
            let response_ident = Ident::new("response", Span::call_site());
            let error_context = quote!(format!("{} {}", "handle_goal_next_request", ACTION_NAME));
            let response_serialization =
                build_response_payload_tokens(&spec, &response_ident, &error_context, None)?;

            action_handle_methods.push(build_handle_goal_next_request(
                goal_request_data_struct.is_some(),
                response_serialization,
            ));
        }

        // Every action hands out a GoalContext with these base methods.
        goal_context_methods.push(build_goal_context_base_methods());

        // Feedback → `GoalContext::publish_feedback`.
        if let Some(feedback) = action.feedback_topic.as_ref() {
            let label = format!("publish_feedback {}", &action.name);
            let struct_prefix = format!("{action_prefix}Feedback");
            let feedback_schema_name = format!("{base_name}_feedback");
            let format_artifacts =
                map_message_format(&feedback_schema_name, feedback.message_format.as_ref())?;
            let params = collect_function_params(
                format_artifacts.as_ref(),
                None,
                &struct_prefix,
                &mut context,
                None,
            )?;
            let encoding = self.prepare_message_encoding(
                &scoped_schema_key(origin, &format!("emit_{base_name}_feedback")),
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            goal_context_methods.push(build_goal_context_publish_feedback(
                &params,
                encoding.as_ref(),
                &label,
            ));
        }

        // Result → `GoalContext::complete` / `complete_cancelled`.
        if let Some(result) = action.result_service.as_ref() {
            let label = format!("{base_name}_result");
            let schema_struct_prefix = format!("{action_prefix}Result");

            let response_artifacts = map_message_format(
                &format!("{label}_response"),
                result.response_message_format.as_ref(),
            )?;

            // The result-response fields become `complete(fields…)` parameters.
            let result_params = collect_function_params(
                response_artifacts.as_ref(),
                None,
                &schema_struct_prefix,
                &mut context,
                None,
            )?;
            let encoding = self.prepare_message_encoding(
                &scoped_schema_key(origin, &format!("{label}_response")),
                &schema_struct_prefix,
                response_artifacts.as_ref(),
                &result_params,
            )?;

            goal_context_methods.push(build_goal_context_complete(
                "complete",
                &result_params,
                encoding.as_ref(),
                &label,
            ));
            goal_context_methods.push(build_goal_context_complete(
                "complete_cancelled",
                &result_params,
                encoding.as_ref(),
                &label,
            ));
        }

        if action_handle_methods.is_empty() {
            return Ok(());
        }

        let action_handle_struct = build_action_handle_struct();
        let goal_context_struct = build_goal_context_struct();
        let expose_method = build_action_expose_method(has_feedback, origin);

        let mut items = vec![quote!(const ACTION_NAME: &str = #action_name_literal;)];
        items.extend(context.into_tokens());
        items.extend(extra_items);
        items.push(action_handle_struct);
        items.push(goal_context_struct);
        items.push(quote! {
            impl ActionHandle {
                #expose_method
                #( #action_handle_methods )*
            }
        });
        items.push(quote! {
            impl GoalContext {
                #( #goal_context_methods )*
            }
        });
        items.extend(helper_tokens);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);
        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&action.name),
            origin,
            InterfaceKind::ExposedAction,
            rendered,
        ));
        Ok(())
    }

    fn add_consumed_topic(
        &mut self,
        topic: &ConsumedTopic,
        arguments: MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let node_name = topic.link_id.as_str();

        let node_component = sanitize_component(node_name);
        let topic_component = sanitize_component(topic.name.as_str());

        debug_assert!(
            !node_component.is_empty(),
            "ConsumedTopic.link_id should be validated as non-empty"
        );
        debug_assert!(
            !topic_component.is_empty(),
            "ConsumedTopic.name should be validated as non-empty"
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

        let schema_key =
            crate::generator::naming::consumed_topic_schema_key(node_name, topic.name.as_str());

        let format_artifacts = map_message_format(&schema_key, Some(&arguments))?
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
        let method_tokens = build_consumed_topic_callback(ConsumedTopicCallbackSpec {
            fn_name: &callback_fn_ident,
            helper_fn_ident: &helper_fn_ident,
            args_struct_ident: &args_struct_ident,
            params: &params,
            artifacts: &format_artifacts,
            encoding: &encoding,
            topic,
            struct_prefix: &message_struct_name,
            dependency,
        })?;
        let mut items = context.into_tokens();
        items.push(method_tokens);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&module_label),
            None,
            InterfaceKind::ConsumedTopic,
            rendered,
        ));

        Ok(())
    }

    fn add_consumed_service(
        &mut self,
        service: &ConsumedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let dependency_node_name = dependency.producer_name.as_str();
        let request_arguments = non_empty_message_format(Some(request_arguments));
        let response_arguments = non_empty_message_format(Some(response_arguments));

        let service_ident = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let service_name_component = service_ident.to_string();

        let request_artifacts = map_message_format(
            &format!("{service_name_component}_request"),
            request_arguments,
        )?;
        let response_artifacts = map_message_format(
            &format!("{service_name_component}_response"),
            response_arguments,
        )?;
        let struct_prefix = to_camel_case(service_name_component.as_str());

        let method_label = crate::generator::naming::consumed_service_request_schema_key(
            dependency_node_name,
            service.name.as_str(),
        );
        let method_ident = Ident::new("poll", Span::call_site());

        let mut context = GenerationContext::default();
        let response_data_ident = Ident::new("ResponseData", Span::call_site());

        let params = collect_function_params(
            request_artifacts.as_ref(),
            response_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
            Some(&response_data_ident),
        )?;

        let instance_id_ident = Ident::new("instance_id", Span::call_site());
        let instance_id_param_index = params
            .iter()
            .position(|param| param.ident == instance_id_ident);

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

        let request_payload_tokens = if let Some(spec) = &request_encoding {
            let unpacking: Vec<_> = request_struct_params
                .iter()
                .map(|p| {
                    let ident = &p.ident;
                    quote!(let #ident = request.#ident;)
                })
                .collect();

            let error_context = quote!(format!("poll {} {}", NODE_NAME, SERVICE_NAME));
            let serialize_block = build_serialize_payload(
                &spec.builder_type,
                &unpacking,
                &spec.assignments,
                &error_context,
            );
            quote! { let request_payload = #serialize_block; }
        } else {
            let suppress_unused = unused_params_stmt(&params);

            quote! {
                #suppress_unused
                let request_payload = peppylib::Payload::new();
            }
        };

        // The `to_target` matches the producer's emission shape: if the
        // dependency exposes the service via `conforms_to`, address it as the
        // interface; otherwise as the dependency's node identity. The
        // `target` — the producer scope, resolved at runtime from the
        // consumer's binding map — pins the full `(core_node, instance_id)`
        // for bound slots (no discovery) and falls back to
        // `ServiceTarget::Any` otherwise.
        let to_target_expr = consumed_to_target_expression(dependency);
        let target_expr =
            crate::generator::rust::topics::consumed_service_target_expression(dependency);
        let poll_call = quote! {
            peppylib::ServiceMessenger::poll(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #to_target_expr,
                SERVICE_NAME,
                #target_expr,
                request_payload,
                timeout,
            )
        };

        let response_struct_ident = Ident::new("Response", Span::call_site());

        let (return_ty, response_tokens, poll_tokens, deserialize_fn_tokens) =
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                let response_struct_name = format!("{struct_prefix}Response");

                let response_schema_key =
                    crate::generator::naming::consumed_service_response_schema_key(
                        dependency_node_name,
                        service.name.as_str(),
                    );
                let response_schema = self.register_schema(
                    &response_schema_key,
                    &response_struct_name,
                    response_artifacts,
                )?;
                let response_reader_type = response_schema.reader_type_tokens();

                let response_format = response_artifacts.message_format();
                let field_context_expr = quote!(context.clone());
                let (response_statements, response_inits, _) = deserialize_format_fields(
                    response_format,
                    &response_struct_name,
                    &field_context_expr,
                )?;

                let poll_tokens = quote! {
                    let response_message = #poll_call.await?;
                };

                context
                    .add_metadata_struct(response_struct_ident.clone(), Some(&response_data_ident));

                let response_tokens = quote! {
                    let payload = response_message.payload();
                    let response_data = deserialize_response(&payload)?;
                    Ok(#response_struct_ident {
                        instance_id: response_message.instance_id().to_string(),
                        core_node: response_message.core_node().to_string(),
                        data: response_data,
                    })
                };

                let svc_context_expr = quote!(format!("{} {} response", NODE_NAME, SERVICE_NAME));
                let deserialize_fn = build_deserialize_fn(
                    &Ident::new("deserialize_response", Span::call_site()),
                    &response_reader_type,
                    &svc_context_expr,
                    &quote!(#response_data_ident),
                    &response_statements,
                    &quote!(#response_data_ident { #( #response_inits ),* }),
                );

                (
                    quote!(#response_struct_ident),
                    response_tokens,
                    poll_tokens,
                    Some(deserialize_fn),
                )
            } else {
                let poll_tokens = quote! {
                    let _ = #poll_call.await?;
                };
                (quote!(()), quote!(Ok(())), poll_tokens, None)
            };

        let mut service_tokens = context.into_tokens();
        let service_name_literal = Literal::string(service.name.as_str());
        let node_name_literal = Literal::string(dependency_node_name);

        let constants_tokens = quote! {
            const NODE_NAME: &str = #node_name_literal;
            const SERVICE_NAME: &str = #service_name_literal;
        };

        let mut fn_param_tokens = vec![
            quote!(node_runner: &crate::NodeRunner),
            quote!(timeout: std::time::Duration),
        ];
        if !request_struct_params.is_empty() {
            fn_param_tokens.push(quote!(request: #request_struct_ident));
        }

        let function_token = quote! {
            pub async fn #method_ident(#(#fn_param_tokens),*) -> crate::Result<#return_ty> {
                #request_payload_tokens

                #poll_tokens

                #response_tokens
            }
        };

        let mut all_tokens = vec![constants_tokens];
        all_tokens.append(&mut service_tokens);
        all_tokens.push(function_token);
        if let Some(deserialize_fn) = deserialize_fn_tokens {
            all_tokens.push(deserialize_fn);
        }

        let mut module_label = raw_module_label(&service.link_id, &service.name);
        if module_name_from_components(&service.link_id, &service.name).is_empty() {
            module_label = method_label
                .strip_prefix("poll_")
                .map(|label| label.to_string())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| method_label.clone());
        }

        let tokens: TokenStream = quote! {
            #( #all_tokens )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&module_label),
            None,
            InterfaceKind::ConsumedService,
            rendered,
        ));
        Ok(())
    }

    fn add_consumed_action(
        &mut self,
        action: &ConsumedAction,
        messages: &ConsumedActionMessage,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let dependency_node_name = dependency.producer_name.as_str();
        let node_component = sanitize_component(dependency_node_name);
        let action_component = sanitize_component(action.name.as_str());
        let base_component = match (node_component.is_empty(), action_component.is_empty()) {
            (true, true) => "action".to_string(),
            (true, false) => action_component.clone(),
            (false, true) => node_component.clone(),
            (false, false) => format!("{node_component}_{action_component}"),
        };
        let action_prefix = to_camel_case(&base_component);
        // `action_struct_name` names Rust IDENTIFIERS in the generated code
        // (e.g. `UvcCameraEnableActionGoalMessage`). The cap'n proto schema
        // keys (file_stems) are produced separately via the shared helper so
        // both the Rust and Python generators emit the same file_stems for
        // the same `(producer, action)` input.
        let action_struct_name = format!("{action_prefix}Action");
        let action_schema_keys = crate::generator::naming::consumed_action_schema_keys(
            dependency_node_name,
            action.name.as_str(),
        );

        let mut context = GenerationContext::default();
        let mut methods: Vec<TokenStream> = Vec::new();
        let mut helper_items: Vec<TokenStream> = Vec::new();

        let node_name_literal = Literal::string(dependency_node_name);
        let action_name_literal = Literal::string(action.name.as_str());
        let constants_tokens = quote! {
            const TARGET_NODE_NAME: &str = #node_name_literal;
            const TARGET_ACTION_NAME: &str = #action_name_literal;
        };

        let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
        // The goal acknowledgement is framework-owned ({accepted, error_message}).
        let goal_response_fmt = goal_action_response_format();
        let goal_response_format = Some(&goal_response_fmt);
        let feedback_format = non_empty_message_format(messages.feedback.as_ref());
        let result_response_format = non_empty_message_format(messages.result_response.as_ref());

        let (goal_method, mut goal_helpers, has_goal_response_data) = self
            .build_consumed_action_fire_goal_method(
                &mut context,
                &action_struct_name,
                goal_request_format,
                goal_response_format,
                &action_schema_keys.goal_request,
                dependency,
            )?;
        methods.push(goal_method);
        helper_items.append(&mut goal_helpers);

        let (cancel_method, mut cancel_helpers) =
            self.build_consumed_action_cancel_method(&mut context)?;
        methods.push(cancel_method);
        helper_items.append(&mut cancel_helpers);

        if let Some(feedback_format) = feedback_format {
            let (feedback_method, mut feedback_helpers) = self
                .build_consumed_action_feedback_method(
                    &mut context,
                    feedback_format,
                    &action_struct_name,
                    &action_schema_keys.feedback,
                )?;
            methods.push(feedback_method);
            helper_items.append(&mut feedback_helpers);
        }

        let result_artifacts =
            map_message_format(&action_schema_keys.result_response, result_response_format)?;
        let (result_method, mut result_helpers) = self.build_consumed_action_get_result_method(
            &mut context,
            &action_struct_name,
            result_artifacts,
            &action_schema_keys.result_response,
        )?;
        methods.push(result_method);
        helper_items.append(&mut result_helpers);

        let goal_response_data_ident = Ident::new("GoalResponseData", Span::call_site());
        let action_handle_struct = if has_goal_response_data {
            quote! {
                pub struct ActionHandle {
                    messenger: peppylib::MessengerHandle,
                    inner: peppylib::messaging::ActionGoalHandle,
                    pub data: #goal_response_data_ident,
                }
            }
        } else {
            quote! {
                pub struct ActionHandle {
                    messenger: peppylib::MessengerHandle,
                    inner: peppylib::messaging::ActionGoalHandle,
                }
            }
        };

        let mut items = vec![constants_tokens];
        items.extend(context.into_tokens());
        items.push(action_handle_struct);
        items.push(quote! {
            impl ActionHandle {
                #( #methods )*
            }
        });
        items.extend(helper_items);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);
        let module_label = raw_module_label(&action.link_id, &action.name);
        self.push_section(self.make_artifact(
            &sanitize_node_display_name(&module_label),
            None,
            InterfaceKind::ConsumedAction,
            rendered,
        ));
        Ok(())
    }

    fn build(
        self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &config::consts::PeppyDirs,
        deploy_mode: crate::generator::common::CrateDeployMode,
    ) -> Result<()> {
        scaffold::add_peppylib_dependencies(&to_path, peppy_dirs, deploy_mode)?;
        scaffold::add_capnp_schemas(&self.schemas, to_path.as_ref())?;
        scaffold::add_artifacts_to_lib(&to_path, self.sections)?;
        scaffold::add_parameters_to_lib(&to_path, &self.parameters)?;
        Ok(())
    }
}

fn prefixed_ident(prefix: &str, candidate: Option<&str>, fallback: &str) -> Ident {
    let name = identifiers::prefixed_name(prefix, candidate, fallback);
    Ident::new(&name, Span::call_site())
}
