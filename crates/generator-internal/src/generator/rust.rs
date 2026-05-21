#[cfg(test)]
mod tests;

mod actions;
mod context;
mod deserialization;
mod identifiers;
pub mod parameters;
mod scaffold;
mod serialization;
mod services;
mod topics;
mod type_mapping;

pub use parameters::{generate_parameters_struct, validate_parameter_schema};

use super::types::{
    CapnpSchema, ConsumedActionMessage, DependencyContext, InterfaceArtifact, InterfaceKind,
    InterfaceOrigin, LanguageGenerator, cancel_action_response_format, non_empty_message_format,
    scoped_schema_key,
};
use crate::error::{Error, Result};
use crate::generator::naming::{
    non_empty_str, resolve_schema_file_stem, sanitize_component, to_camel_case,
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
    ActionHandleRole, build_action_expose_method, build_action_feedback_emit,
    build_action_handle_method, build_action_handle_struct, build_action_payload_handler,
    build_action_request_deserializer,
};
use context::{GenerationContext, collect_function_params, map_message_format};
use deserialization::{build_deserialize_fn, deserialize_format_fields};
use serialization::{
    MessageEncodingSpec, build_serialize_payload, generate_assignments_for_format,
    generate_assignments_from_struct,
};
use services::{
    ExposedServiceMethodSpec, ServiceResponseSpec, build_exposed_service_method,
    build_request_struct_with_name_and_impl, deserialize_fields_from_format,
};
use topics::{
    ConsumedTopicCallbackSpec, build_consumed_topic_callback, build_topic_emit,
    consumed_to_target_expression,
};
use type_mapping::{render_tokens, unused_params_stmt};

/// Rust-specific implementation of the interface generator.
#[derive(Default)]
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
    parameters: config::ParameterSchema,
    /// Per-`(producer_name, producer_tag)` set of pinned sibling link_ids
    /// declared by the node's `depends_on`. Populated by [`Self::set_pinned_siblings_map`]
    /// before codegen runs; the [`build`] step emits a single
    /// `register_consumer_dependencies` scaffold file that consumer functions
    /// call to seed the messenger's sibling-precedence table on first use.
    pinned_siblings_map: HashMap<(String, String), Vec<String>>,
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

    /// Seeds the per-`(name, tag)` pinned-sibling map. Consumed-interface
    /// codegen emits an `ensure_dependencies_registered` scaffold that
    /// installs this map onto the runtime `MessengerHandle` so from_any
    /// consumers learn which producer link_ids their pinned siblings claim.
    pub fn set_pinned_siblings_map(&mut self, map: HashMap<(String, String), Vec<String>>) {
        self.pinned_siblings_map = map;
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
        let schema_source = artifacts.encoding_schema();
        let resolved = resolve_schema_file_stem(schema_key);
        let struct_name = format!("{struct_prefix}Message");
        let struct_module = crate::generator::naming::normalize_snake_case(&struct_name);
        let schema = schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let capnp_schema = CapnpSchema::new(resolved.file_stem.clone(), schema);
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
        // `conforms_to`, otherwise as its native Node identity.
        let to_target_expr = consumed_to_target_expression(dependency);
        let to_link_id_expr =
            crate::generator::rust::topics::consumed_from_link_id_expression(dependency);
        // Same gating as consumed services: expose `target_instance_id` only
        // for wildcard (`from_any: true`) deps. Pinned deps already route to
        // exactly one producer.
        let expose_target_instance_id = dependency.link_id.is_wildcard();
        let target_instance_id_param = if expose_target_instance_id {
            quote!(target_instance_id: Option<&str>,)
        } else {
            quote!()
        };
        let target_instance_id_arg = if expose_target_instance_id {
            quote!(target_instance_id)
        } else {
            quote!(None)
        };
        let method_tokens = quote! {
            pub async fn fire_goal(
                node_runner: &crate::NodeRunner,
                timeout: std::time::Duration,
                #target_instance_id_param
                #request_param
                feedback_qos: peppylib::config::QoSProfile,
            ) -> crate::Result<Self> {
                crate::consumer_dependencies::ensure_registered(node_runner.messenger());

                #goal_payload_tokens

                let action_handle = peppylib::ActionMessenger::send_goal(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    #to_target_expr,
                    #to_link_id_expr,
                    TARGET_ACTION_NAME,
                    None,
                    #target_instance_id_arg,
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

    /// Builds a consumed action response method (cancel_goal or get_result).
    ///
    /// Both follow the same structure: register response artifacts, build a deserializer,
    /// and generate a method that calls the appropriate ActionMessenger function.
    fn build_consumed_action_response_method(
        &mut self,
        context: &mut GenerationContext,
        spec: ConsumedActionResponseMethodSpec<'_>,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let response_data_ident = Ident::new(spec.data_struct_name, Span::call_site());
        let response_ident = Ident::new(spec.response_struct_name, Span::call_site());
        let method_name_ident = Ident::new(spec.method_name, Span::call_site());
        let deserializer_ident = Ident::new(spec.deserializer_name, Span::call_site());

        let mut helper_items = Vec::new();
        let method_tokens = if let Some(response_artifacts) = spec.response_artifacts {
            let _response_params = collect_function_params(
                None,
                Some(&response_artifacts),
                &format!("{}{}", spec.action_struct_name, spec.response_struct_name),
                context,
                Some(&response_data_ident),
            )?;

            context.add_metadata_struct(response_ident.clone(), Some(&response_data_ident));

            let response_schema_key = format!("{}_response", spec.schema_key);
            let response_schema = self.register_schema(
                &response_schema_key,
                spec.schema_message_prefix,
                &response_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();

            let response_format = response_artifacts.message_format();
            let data_label = spec.data_struct_name;
            let response_label = spec.response_struct_name;
            let context_expr = quote!(format!(
                "{} {} {}",
                TARGET_NODE_NAME, TARGET_ACTION_NAME, #response_label
            ));
            let (response_statements, response_inits, _) =
                deserialize_format_fields(response_format, data_label, &context_expr)?;

            let deserialize_helper = build_deserialize_fn(
                &deserializer_ident,
                &reader_type,
                &context_expr,
                &quote!(#response_data_ident),
                &response_statements,
                &quote!(#response_data_ident { #( #response_inits ),* }),
            );
            helper_items.push(deserialize_helper);

            let messenger_call = &spec.messenger_call;
            quote! {
                pub async fn #method_name_ident(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<#response_ident> {
                    let response = #messenger_call.await?;

                    let payload = response.payload();
                    let response_data = #deserializer_ident(payload.as_ref())?;
                    Ok(#response_ident {
                        instance_id: response.instance_id().to_string(),
                        core_node: response.core_node().to_string(),
                        data: response_data,
                    })
                }
            }
        } else {
            context.add_metadata_struct(response_ident.clone(), None);

            let messenger_call = &spec.messenger_call;
            quote! {
                pub async fn #method_name_ident(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<#response_ident> {
                    let response = #messenger_call.await?;

                    Ok(#response_ident {
                        instance_id: response.instance_id().to_string(),
                        core_node: response.core_node().to_string(),
                    })
                }
            }
        };

        Ok((method_tokens, helper_items))
    }

    /// Builds the on_next_feedback_message method for consumed actions
    fn build_consumed_action_feedback_method(
        &mut self,
        context: &mut GenerationContext,
        format: &MessageFormat,
        action_struct_name: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let feedback_schema_name = format!("{action_struct_name}_feedback");
        let format_artifacts = map_message_format(&feedback_schema_name, Some(format))?
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

/// Describes the parameterizable parts of a consumed action response method
/// (cancel_goal or get_result).
struct ConsumedActionResponseMethodSpec<'a> {
    action_struct_name: &'a str,
    method_name: &'a str,
    response_struct_name: &'a str,
    data_struct_name: &'a str,
    deserializer_name: &'a str,
    schema_message_prefix: &'a str,
    schema_key: &'a str,
    response_artifacts: Option<CapnpSchemaArtifacts>,
    messenger_call: TokenStream,
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
        let method_ident = Ident::new("emit", Span::call_site());
        let method_tokens = build_topic_emit(
            &method_ident,
            &params,
            encoding.as_ref(),
            topic,
            &fn_name_str,
            origin,
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
        let mut action_handle_methods: Vec<TokenStream> = Vec::new();
        let mut helper_tokens: Vec<TokenStream> = Vec::new();

        let action_name_literal = Literal::string(&action.name);

        let has_goal = action.goal_service.is_some();
        let has_feedback = action.feedback_topic.is_some();
        let has_result = action.result_service.is_some();

        if let Some(goal) = action.goal_service.as_ref() {
            let label = format!("{base_name}_goal");
            let schema_struct_prefix = format!("{action_prefix}Goal");

            let request_artifacts = map_message_format(
                &format!("{label}_request"),
                goal.request_message_format.as_ref(),
            )?;
            let response_artifacts = map_message_format(
                &format!("{label}_response"),
                goal.response_message_format.as_ref(),
            )?;

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

            let response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
                let response_schema_prefix = format!("{schema_struct_prefix}Response");
                let schema_key = scoped_schema_key(origin, &format!("{label}_response"));
                let schema_info =
                    self.register_schema(&schema_key, &response_schema_prefix, return_artifacts)?;
                Some(ServiceResponseSpec {
                    format: return_artifacts.message_format(),
                    struct_ident: Ident::new("GoalResponse", Span::call_site()),
                    builder_type: schema_info.builder_type_tokens(),
                    include_service_instance_id: false,
                })
            } else {
                None
            };

            if let Some(spec) = encoding.as_ref() {
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

            let goal_handler_fn = build_action_payload_handler(
                &Ident::new("handle_goal_payload", Span::call_site()),
                &Ident::new("deserialize_goal_request", Span::call_site()),
                &Ident::new("GoalRequest", Span::call_site()),
                goal_request_data_struct.as_ref(),
                response_spec.as_ref(),
                encoding.is_some(),
            )?;
            helper_tokens.push(goal_handler_fn);

            let goal_role = if has_feedback {
                ActionHandleRole::Goal
            } else {
                ActionHandleRole::Plain
            };
            let goal_method = build_action_handle_method(
                &Ident::new("handle_goal_next_request", Span::call_site()),
                &Ident::new("handle_goal_payload", Span::call_site()),
                &Ident::new("GoalRequest", Span::call_site()),
                &Ident::new("GoalResponse", Span::call_site()),
                &Ident::new("goal_service", Span::call_site()),
                encoding.is_some(),
                goal_role,
            );
            action_handle_methods.push(goal_method);

            let cancel_label = format!("{base_name}_goal_cancel");
            let cancel_schema_prefix = format!("{action_prefix}GoalCancel");
            let cancel_response_format = cancel_action_response_format();
            let cancel_response_artifacts =
                map_message_format(&cancel_label, Some(&cancel_response_format))?;

            context.add_metadata_struct(Ident::new("CancelRequest", Span::call_site()), None);

            let _cancel_params = collect_function_params(
                None,
                cancel_response_artifacts.as_ref(),
                "Cancel",
                &mut context,
                None,
            )?;

            let cancel_response_spec =
                if let Some(return_artifacts) = cancel_response_artifacts.as_ref() {
                    let schema_key = scoped_schema_key(origin, &format!("{cancel_label}_response"));
                    let schema_info = self.register_schema(
                        &schema_key,
                        &format!("{cancel_schema_prefix}Response"),
                        return_artifacts,
                    )?;
                    Some(ServiceResponseSpec {
                        format: return_artifacts.message_format(),
                        struct_ident: Ident::new("CancelResponse", Span::call_site()),
                        builder_type: schema_info.builder_type_tokens(),
                        include_service_instance_id: false,
                    })
                } else {
                    None
                };

            let cancel_handler_fn = build_action_payload_handler(
                &Ident::new("handle_cancel_payload", Span::call_site()),
                &Ident::new("deserialize_cancel_request", Span::call_site()),
                &Ident::new("CancelRequest", Span::call_site()),
                None,
                cancel_response_spec.as_ref(),
                false,
            )?;
            helper_tokens.push(cancel_handler_fn);

            let cancel_role = if has_feedback {
                ActionHandleRole::Cancel
            } else {
                ActionHandleRole::Plain
            };
            let cancel_method = build_action_handle_method(
                &Ident::new("handle_cancel_next_request", Span::call_site()),
                &Ident::new("handle_cancel_payload", Span::call_site()),
                &Ident::new("CancelRequest", Span::call_site()),
                &Ident::new("CancelResponse", Span::call_site()),
                &Ident::new("cancel_service", Span::call_site()),
                false,
                cancel_role,
            );
            action_handle_methods.push(cancel_method);
        }

        if let Some(result) = action.result_service.as_ref() {
            let label = format!("{base_name}_result");
            let schema_struct_prefix = format!("{action_prefix}Result");

            let response_artifacts = map_message_format(
                &format!("{label}_response"),
                result.response_message_format.as_ref(),
            )?;

            context.add_metadata_struct(Ident::new("ResultRequest", Span::call_site()), None);

            let _result_params = collect_function_params(
                None,
                response_artifacts.as_ref(),
                "Result",
                &mut context,
                None,
            )?;

            let result_response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
                let schema_key = scoped_schema_key(origin, &format!("{label}_response"));
                let schema_info = self.register_schema(
                    &schema_key,
                    &format!("{schema_struct_prefix}Response"),
                    return_artifacts,
                )?;
                Some(ServiceResponseSpec {
                    format: return_artifacts.message_format(),
                    struct_ident: Ident::new("ResultResponse", Span::call_site()),
                    builder_type: schema_info.builder_type_tokens(),
                    include_service_instance_id: false,
                })
            } else {
                None
            };

            let result_handler_fn = build_action_payload_handler(
                &Ident::new("handle_result_payload", Span::call_site()),
                &Ident::new("deserialize_result_request", Span::call_site()),
                &Ident::new("ResultRequest", Span::call_site()),
                None,
                result_response_spec.as_ref(),
                false,
            )?;
            helper_tokens.push(result_handler_fn);

            let result_role = if has_feedback {
                ActionHandleRole::Result
            } else {
                ActionHandleRole::Plain
            };
            let result_method = build_action_handle_method(
                &Ident::new("handle_result_next_request", Span::call_site()),
                &Ident::new("handle_result_payload", Span::call_site()),
                &Ident::new("ResultRequest", Span::call_site()),
                &Ident::new("ResultResponse", Span::call_site()),
                &Ident::new("result_service", Span::call_site()),
                false,
                result_role,
            );
            action_handle_methods.push(result_method);
        }

        if let Some(feedback) = action.feedback_topic.as_ref() {
            let label = format!("emit_feedback {}", &action.name);
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

            let method_tokens = build_action_feedback_emit(&params, encoding.as_ref(), &label);
            action_handle_methods.push(method_tokens);
        }

        if action_handle_methods.is_empty() {
            return Ok(());
        }

        let action_handle_struct = build_action_handle_struct(has_goal, has_feedback, has_result);
        let expose_method = build_action_expose_method(has_goal, has_feedback, has_result, origin);

        let mut items = vec![quote!(const ACTION_NAME: &str = #action_name_literal;)];
        items.extend(context.into_tokens());
        items.push(action_handle_struct);
        items.push(quote! {
            impl ActionHandle {
                #expose_method
                #( #action_handle_methods )*
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
        let ConsumedTopic::Linked(linked) = topic else {
            return Err(Error::InvariantViolation {
                context: "add_consumed_topic called with ConsumedTopic::External; use add_external_consumed_topic instead".into(),
            });
        };
        let node_name = linked.link_id.as_str();

        let node_component = sanitize_component(node_name);
        let topic_component = sanitize_component(linked.name.as_str());

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

        let mut module_label = format!("{}_{}", node_name, linked.name.as_str());
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

    fn add_external_consumed_topic(&mut self, name: &str, arguments: MessageFormat) -> Result<()> {
        let topic_component = sanitize_component(name);

        debug_assert!(
            !topic_component.is_empty(),
            "External consumed topic name should be validated as non-empty"
        );

        let mut struct_prefix = to_camel_case(&topic_component);
        if struct_prefix.is_empty() {
            struct_prefix = String::from("Topic");
        }

        let module_label = name.trim().to_string();
        let schema_key = format!("on_next_{topic_component}_message");

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
        let method_tokens = topics::build_external_consumed_topic_callback(
            topics::ExternalConsumedTopicCallbackSpec {
                fn_name: &callback_fn_ident,
                helper_fn_ident: &helper_fn_ident,
                args_struct_ident: &args_struct_ident,
                params: &params,
                artifacts: &format_artifacts,
                encoding: &encoding,
                topic_name: name,
                struct_prefix: &message_struct_name,
            },
        )?;
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

        let method_label = {
            let mut components = Vec::with_capacity(3);
            components.push(String::from("poll"));

            let node_ident = prefixed_ident("", Some(dependency_node_name), "node");
            components.push(node_ident.to_string());

            components.push(service_name_component.clone());

            components.join("_")
        };
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
        // interface; otherwise as the dependency's node identity.
        let to_target_expr = consumed_to_target_expression(dependency);
        let to_link_id_expr =
            crate::generator::rust::topics::consumed_from_link_id_expression(dependency);
        // `target_instance_id` is exposed to the caller only when the
        // dependency is wildcard (`from_any: true`): pinned deps already
        // route to exactly one producer via the link_id literal and have
        // nothing more for the caller to address. `target_core_node` is
        // never exposed in the generated API.
        let expose_target_instance_id = dependency.link_id.is_wildcard();
        let target_instance_id_arg = if expose_target_instance_id {
            quote!(target_instance_id)
        } else {
            quote!(None)
        };
        let poll_call = quote! {
            peppylib::ServiceMessenger::poll(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #to_target_expr,
                #to_link_id_expr,
                SERVICE_NAME,
                None,
                #target_instance_id_arg,
                request_payload,
                timeout,
            )
        };

        let response_struct_ident = Ident::new("Response", Span::call_site());

        let (return_ty, response_tokens, poll_tokens, deserialize_fn_tokens) =
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                let response_struct_name = format!("{struct_prefix}Response");

                let response_schema_key = format!("{method_label}_response");
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
        if expose_target_instance_id {
            fn_param_tokens.push(quote!(target_instance_id: Option<&str>));
        }
        if !request_struct_params.is_empty() {
            fn_param_tokens.push(quote!(request: #request_struct_ident));
        }

        let function_token = quote! {
            pub async fn #method_ident(#(#fn_param_tokens),*) -> crate::Result<#return_ty> {
                crate::consumer_dependencies::ensure_registered(node_runner.messenger());

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
        let action_struct_name = format!("{action_prefix}Action");

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
        let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
        let feedback_format = non_empty_message_format(messages.feedback.as_ref());
        let result_response_format = non_empty_message_format(messages.result_response.as_ref());

        let goal_schema_key = format!("{action_struct_name}_fire_goal");
        let (goal_method, mut goal_helpers, has_goal_response_data) = self
            .build_consumed_action_fire_goal_method(
                &mut context,
                &action_struct_name,
                goal_request_format,
                goal_response_format,
                &goal_schema_key,
                dependency,
            )?;
        methods.push(goal_method);
        helper_items.append(&mut goal_helpers);

        let cancel_schema_key = format!("{action_struct_name}_cancel_goal");
        let cancel_response_format = cancel_action_response_format();
        let cancel_artifacts =
            map_message_format(&cancel_schema_key, Some(&cancel_response_format))?
                .expect("cancel response format should yield artifacts");
        let (cancel_method, mut cancel_helpers) = self.build_consumed_action_response_method(
            &mut context,
            ConsumedActionResponseMethodSpec {
                action_struct_name: &action_struct_name,
                method_name: "cancel_goal",
                response_struct_name: "CancelResponse",
                data_struct_name: "CancelResponseData",
                deserializer_name: "deserialize_cancel_response",
                schema_message_prefix: "CancelResponseMessage",
                schema_key: &cancel_schema_key,
                response_artifacts: Some(cancel_artifacts),
                messenger_call: quote! {
                    peppylib::ActionMessenger::cancel_goal(
                        &self.messenger,
                        &self.inner,
                        timeout,
                    )
                },
            },
        )?;
        methods.push(cancel_method);
        helper_items.append(&mut cancel_helpers);

        if let Some(feedback_format) = feedback_format {
            let (feedback_method, mut feedback_helpers) = self
                .build_consumed_action_feedback_method(
                    &mut context,
                    feedback_format,
                    &action_struct_name,
                )?;
            methods.push(feedback_method);
            helper_items.append(&mut feedback_helpers);
        }

        let result_schema_key = format!("{action_struct_name}_get_result");
        let result_artifacts = map_message_format(
            &format!("{result_schema_key}_response"),
            result_response_format,
        )?;
        let (result_method, mut result_helpers) = self.build_consumed_action_response_method(
            &mut context,
            ConsumedActionResponseMethodSpec {
                action_struct_name: &action_struct_name,
                method_name: "get_result",
                response_struct_name: "ResultResponse",
                data_struct_name: "ResultResponseData",
                deserializer_name: "deserialize_result_response",
                schema_message_prefix: "ResultResponseMessage",
                schema_key: &result_schema_key,
                response_artifacts: result_artifacts,
                messenger_call: quote! {
                    peppylib::ActionMessenger::request_result(
                        &self.messenger,
                        &self.inner,
                        timeout,
                    )
                },
            },
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
        scaffold::add_consumer_dependencies_to_lib(&to_path, &self.pinned_siblings_map)?;
        Ok(())
    }
}

fn prefixed_ident(prefix: &str, candidate: Option<&str>, fallback: &str) -> Ident {
    let name = identifiers::prefixed_name(prefix, candidate, fallback);
    Ident::new(&name, Span::call_site())
}

use super::naming::{module_name_from_components, raw_module_label, sanitize_node_display_name};
