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
        ArraySchema, ExposedAction, ExposedService, ExposedTopic, MessageFormat, QoSProfile,
        SchemaType, SubscribedAction, SubscribedService, SubscribedTopic, TypeToken,
    },
};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use syn::{File, parse2};

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
    pending_exposed_services: Option<ExposedServicesModule>,
    pending_exposed_topics: Option<ExposedTopicsModule>,
    pending_subscribed_services: BTreeMap<String, SubscribedServiceNode>,
    pending_subscribed_topics: BTreeMap<String, SubscribedTopicNode>,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
            schemas: HashMap::new(),
            pending_exposed_services: None,
            pending_exposed_topics: None,
            pending_subscribed_services: BTreeMap::new(),
            pending_subscribed_topics: BTreeMap::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(mut self) -> Vec<InterfaceArtifact> {
        self.flush_pending_exposed_services();
        self.flush_pending_exposed_topics();
        self.flush_pending_subscribed_services();
        self.flush_pending_subscribed_topics();
        self.sections
    }

    fn flush_pending_exposed_services(&mut self) {
        if let Some(module) = self.pending_exposed_services.take() {
            self.push_section(module.into_artifact());
        }
    }

    fn flush_pending_exposed_topics(&mut self) {
        if let Some(module) = self.pending_exposed_topics.take() {
            self.push_section(module.into_artifact());
        }
    }

    fn flush_pending_subscribed_services(&mut self) {
        let pending = std::mem::take(&mut self.pending_subscribed_services);
        for (_node_name, node) in pending {
            let artifact = node.into_artifact();
            self.push_section(artifact);
        }
    }

    fn flush_pending_subscribed_topics(&mut self) {
        let pending = std::mem::take(&mut self.pending_subscribed_topics);
        for (_node_name, node) in pending {
            let artifact = node.into_artifact();
            self.push_section(artifact);
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
    #[allow(dead_code)] // TODO: Use this for proper response serialization
    format: &'a MessageFormat,
    struct_ident: Ident,
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

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let format_artifacts = map_message_format(topic.message_format.as_ref())?;
        let params = collect_function_params(
            format_artifacts.as_ref(),
            None,
            &struct_prefix,
            &mut context,
        )?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &struct_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();
        let method_ident = Ident::new(&format!("emit_{fn_name_str}"), Span::call_site());
        let method_tokens = build_topic_emit(
            &method_ident,
            &params,
            encoding.as_ref(),
            topic,
            &fn_name_str,
        );

        let module = self
            .pending_exposed_topics
            .get_or_insert_with(ExposedTopicsModule::new);
        module.ensure_node_name(topic.name.as_str());
        module.extend_structs(struct_tokens);
        module.push_method(method_tokens);
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let fn_name_str = fn_name.to_string();
        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let accept_format_artifacts = map_message_format(service.request_message_format.as_ref())?;
        let return_format_artifacts = map_message_format(service.response_message_format.as_ref())?;
        let params = collect_function_params(
            accept_format_artifacts.as_ref(),
            return_format_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
        )?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &struct_prefix,
            accept_format_artifacts.as_ref(),
            &params,
        )?;

        let request_struct_ident =
            if let Some((ident, tokens)) = build_request_struct(&struct_prefix, &params) {
                context.add_private_struct(tokens);
                Some(ident)
            } else {
                None
            };

        let response_spec = if let Some(return_artifacts) = return_format_artifacts.as_ref() {
            let response_prefix = format!("{struct_prefix}Response");
            let schema_key = format!("{fn_name_str}_response");
            self.register_schema(&schema_key, &response_prefix, return_artifacts)?;
            Some(ServiceResponseSpec {
                format: return_artifacts.message_format(),
                struct_ident: Ident::new(&response_prefix, Span::call_site()),
            })
        } else {
            None
        };

        let struct_tokens = context.into_tokens();

        let service_name_literal = Literal::string(service.name.as_str());
        let (method_token, helper_tokens) = build_exposed_service_method(
            &fn_name,
            &params,
            encoding.as_ref(),
            &fn_name_str,
            &service_name_literal,
            request_struct_ident.as_ref(),
            response_spec.as_ref(),
        );

        let module = self
            .pending_exposed_services
            .get_or_insert_with(ExposedServicesModule::new);
        module.ensure_node_name(service.name.as_str());
        module.extend_structs(struct_tokens);
        module.push_method(method_token, helper_tokens);
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let base_ident = prefixed_ident("", non_empty_str(&action.name), "action");
        let base_name = base_ident.to_string();

        let mut context = GenerationContext::default();
        let mut function_blocks: Vec<TokenStream> = Vec::new();

        if let Some(goal) = action.goal_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_goal"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Goal", to_camel_case(&base_name));
            let accept_format_artifacts = map_message_format(goal.accept_message_format.as_ref())?;
            let return_format_artifacts =
                map_message_format(goal.response_message_format.as_ref())?;
            let params = collect_function_params(
                accept_format_artifacts.as_ref(),
                return_format_artifacts.as_ref(),
                &struct_prefix,
                &mut context,
            )?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                accept_format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(feedback) = action.feedback_topic.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_feedback"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Feedback", to_camel_case(&base_name));
            let format_artifacts = map_message_format(feedback.message_format.as_ref())?;
            let params = collect_function_params(
                format_artifacts.as_ref(),
                None,
                &struct_prefix,
                &mut context,
            )?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(result) = action.result_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_result"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Result", to_camel_case(&base_name));
            let accept_format_artifacts =
                map_message_format(result.accept_message_format.as_ref())?;
            let return_format_artifacts =
                map_message_format(result.response_message_format.as_ref())?;
            let params = collect_function_params(
                accept_format_artifacts.as_ref(),
                return_format_artifacts.as_ref(),
                &struct_prefix,
                &mut context,
            )?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                accept_format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if function_blocks.is_empty() {
            return Ok(());
        }

        let struct_tokens = context.into_tokens();

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Actions {
                #( #function_blocks )*
            }
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
        arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        let format_artifacts = map_message_format(arguments)?
            .ok_or_else(|| Error::SubscriberTopicMessageFormatMissing(topic.name.clone()))?;

        let node_component = sanitize_component(topic.node.as_deref().unwrap_or(""));
        let topic_component = sanitize_component(topic.name.as_str());

        if topic.node.is_some() {
            debug_assert!(
                !node_component.is_empty(),
                "SubscribedTopic.node should be validated as non-empty"
            );
        }
        debug_assert!(
            !topic_component.is_empty(),
            "SubscribedTopic.name should be validated as non-empty"
        );

        let node_prefix = if node_component.is_empty() {
            String::new()
        } else {
            to_camel_case(&node_component)
        };
        let topic_prefix = to_camel_case(&topic_component);
        let struct_prefix = format!("{node_prefix}{topic_prefix}");
        let message_struct_name = format!("{struct_prefix}Message");

        let callback_fn_name = match (node_component.is_empty(), topic_component.is_empty()) {
            (true, true) => "on_next_message".to_string(),
            (true, false) => format!("on_next_{topic_component}_message"),
            (false, true) => format!("on_next_{node_component}_message"),
            (false, false) => format!("on_next_{node_component}_{topic_component}_message"),
        };
        let callback_fn_ident = Ident::new(&callback_fn_name, Span::call_site());

        let mut context = GenerationContext::default();
        let params = collect_function_params(
            Some(&format_artifacts),
            None,
            &message_struct_name,
            &mut context,
        )?;

        let args_struct_ident = Ident::new(&message_struct_name, Span::call_site());
        let args_fields: Vec<(Ident, TokenStream)> = params
            .iter()
            .map(|param| (param.ident.clone(), param.ty.clone()))
            .collect();
        context.add_struct(args_struct_ident.clone(), args_fields);

        let schema_key = if topic_component.is_empty() {
            callback_fn_name.clone()
        } else {
            format!("on_next_{topic_component}_message")
        };

        let encoding = self
            .prepare_message_encoding(
                &schema_key,
                &struct_prefix,
                Some(&format_artifacts),
                &params,
            )?
            .expect("message encoding spec should exist when message format is provided");
        let struct_tokens = context.into_tokens();
        let node_key = topic.node.clone().unwrap_or_else(|| topic.name.clone());
        let node_entry = self
            .pending_subscribed_topics
            .entry(node_key.clone())
            .or_insert_with(|| SubscribedTopicNode::new(node_key.clone()));

        node_entry.extend_message_structs(struct_tokens);

        let method_tokens = build_subscribed_topic_callback(
            &callback_fn_ident,
            &args_struct_ident,
            &params,
            &format_artifacts,
            &encoding,
            topic,
            &message_struct_name,
        );
        node_entry.push_method(method_tokens);

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

        let method_ident = {
            let mut components = Vec::with_capacity(3);
            components.push(String::from("poll"));

            if let Some(node_name) = service.node.as_deref().and_then(non_empty_str) {
                let node_ident = prefixed_ident("", Some(node_name), "node");
                components.push(node_ident.to_string());
            }

            components.push(service_name_component.clone());

            let method_name = components.join("_");
            Ident::new(&method_name, Span::call_site())
        };

        let method_name = method_ident.to_string();

        let mut context = GenerationContext::default();

        let params = collect_function_params(
            request_artifacts.as_ref(),
            response_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
        )?;

        let request_encoding = self.prepare_message_encoding(
            &method_name,
            &struct_prefix,
            request_artifacts.as_ref(),
            &params,
        )?;

        let request_context_literal = Literal::string(&method_name);

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
            crate::messaging::ServiceMessenger::poll(
                messenger.handle(),
                namespace,
                service_name,
                request_payload,
                std::time::Duration::from_secs(3),
            )
        };

        let (return_ty, response_tokens, poll_tokens) =
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                let response_struct_name = format!("{struct_prefix}Response");
                let response_struct_ident = Ident::new(&response_struct_name, Span::call_site());

                let response_schema_key = format!("{method_name}_response");
                let response_schema = self.register_schema(
                    &response_schema_key,
                    &response_struct_name,
                    response_artifacts,
                )?;
                let response_reader_type = response_schema.reader_type_tokens();

                let response_format = response_artifacts.message_format();
                let mut response_statements = Vec::new();
                let mut response_inits = Vec::new();
                let mut name_gen = NameGenerator::new();
                for (field_name, schema) in &response_format.0 {
                    let (mut statements, value_ident) = generate_field_reader_statements(
                        &quote!(root),
                        field_name.as_str(),
                        schema,
                        &response_struct_name,
                        &mut name_gen,
                    );
                    response_statements.append(&mut statements);
                    let field_ident =
                        Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                    response_inits.push(quote!(#field_ident: #value_ident));
                }

                let response_context_literal = Literal::string(&response_struct_name);

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
                        .get_root::<#response_reader_type>()
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

        let struct_tokens = context.into_tokens();
        let service_name_literal = Literal::string(service.name.as_str());

        let mut fn_param_tokens = vec![quote!(messenger: &crate::Messenger)];
        fn_param_tokens.extend(function_param_tokens(&params));

        let function_token = quote! {
            pub async fn #method_ident(#(#fn_param_tokens),*) -> crate::Result<#return_ty> {
                let namespace = messenger.namespace();
                let service_name = #service_name_literal;

                #request_payload_tokens

                #poll_tokens

                #response_tokens
            }
        };

        let node_key = service.node.clone().unwrap_or_else(|| service.name.clone());
        let entry = self
            .pending_subscribed_services
            .entry(node_key.clone())
            .or_insert_with(|| SubscribedServiceNode::new(node_key.clone()));

        entry.extend_message_structs(struct_tokens);
        entry.push_method(function_token);
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: Option<&SubscribedActionMessage>,
    ) -> Result<()> {
        let base_ident = prefixed_ident("on", non_empty_str(action.name.as_str()), "action");
        let base_name = base_ident.to_string();
        let mut struct_blocks: Vec<TokenStream> = Vec::new();
        let mut function_blocks: Vec<TokenStream> = Vec::new();

        let feedback_fn_name = Ident::new(&(base_name.clone() + "_feedback"), Span::call_site());
        let feedback_format = messages.map(|msg| &msg.feedback);
        let (feedback_structs, feedback_fn) = build_async_returning_function(
            &feedback_fn_name,
            feedback_format,
            "Arguments",
            "await for action feedback with PMI",
        )?;
        struct_blocks.extend(feedback_structs);
        function_blocks.push(feedback_fn);

        let result_fn_name = Ident::new(&(base_name + "_result"), Span::call_site());
        let result_format = messages.map(|msg| &msg.result);
        let (result_structs, result_fn) = build_async_returning_function(
            &result_fn_name,
            result_format,
            "Arguments",
            "await for action result with PMI",
        )?;
        struct_blocks.extend(result_structs);
        function_blocks.push(result_fn);

        if function_blocks.is_empty() {
            return Ok(());
        }

        let tokens: TokenStream = quote! {
            #( #struct_blocks )*

            impl Actions {
                #( #function_blocks )*
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::SubscribedAction,
            rendered,
        ));
        Ok(())
    }

    fn build(mut self, to_path: impl AsRef<Path>) -> Result<()> {
        self.flush_pending_exposed_services();
        self.flush_pending_exposed_topics();
        self.flush_pending_subscribed_services();
        self.flush_pending_subscribed_topics();

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

struct ExposedServicesModule {
    node_name: String,
    struct_ident: Ident,
    message_structs: Vec<TokenStream>,
    impl_items: Vec<TokenStream>,
}

impl ExposedServicesModule {
    fn new() -> Self {
        Self {
            node_name: String::new(),
            struct_ident: Ident::new("Exposes", Span::call_site()),
            message_structs: Vec::new(),
            impl_items: Vec::new(),
        }
    }

    fn ensure_node_name(&mut self, name: &str) {
        if self.node_name.is_empty() {
            self.node_name = name.to_string();
        }
    }

    fn extend_structs(&mut self, structs: Vec<TokenStream>) {
        self.message_structs.extend(structs);
    }

    fn push_method(&mut self, method: TokenStream, helpers: Vec<TokenStream>) {
        self.impl_items.push(method);
        self.impl_items.extend(helpers);
    }

    fn into_artifact(self) -> InterfaceArtifact {
        let ExposedServicesModule {
            node_name,
            struct_ident,
            message_structs,
            impl_items,
        } = self;

        let struct_tokens = build_service_struct(&struct_ident);

        let tokens: TokenStream = quote! {
            #( #message_structs )*

            #struct_tokens

            impl #struct_ident {
                #( #impl_items )*
            }
        };

        let rendered = render_tokens(tokens);

        InterfaceArtifact::from_kind(&node_name, InterfaceKind::ExposedService, rendered)
    }
}

struct ExposedTopicsModule {
    node_name: String,
    struct_ident: Ident,
    message_structs: Vec<TokenStream>,
    methods: Vec<TokenStream>,
}

impl ExposedTopicsModule {
    fn new() -> Self {
        Self {
            node_name: String::new(),
            struct_ident: Ident::new("Exposes", Span::call_site()),
            message_structs: Vec::new(),
            methods: Vec::new(),
        }
    }

    fn ensure_node_name(&mut self, name: &str) {
        if self.node_name.is_empty() {
            self.node_name = name.to_string();
        }
    }

    fn extend_structs(&mut self, structs: Vec<TokenStream>) {
        self.message_structs.extend(structs);
    }

    fn push_method(&mut self, method: TokenStream) {
        self.methods.push(method);
    }

    fn into_artifact(self) -> InterfaceArtifact {
        let ExposedTopicsModule {
            node_name,
            struct_ident,
            message_structs,
            methods,
        } = self;

        let struct_tokens = build_topic_struct(&struct_ident);

        let tokens: TokenStream = quote! {
            #( #message_structs )*

            #struct_tokens

            impl #struct_ident {
                #( #methods )*
            }
        };

        let rendered = render_tokens(tokens);

        InterfaceArtifact::from_kind(&node_name, InterfaceKind::ExposedTopic, rendered)
    }
}

struct SubscribedTopicNode {
    node_name: String,
    message_structs: Vec<TokenStream>,
    methods: Vec<TokenStream>,
}

struct SubscribedServiceNode {
    node_name: String,
    message_structs: Vec<TokenStream>,
    methods: Vec<TokenStream>,
}

impl SubscribedServiceNode {
    fn new(node_name: String) -> Self {
        Self {
            node_name,
            message_structs: Vec::new(),
            methods: Vec::new(),
        }
    }

    fn extend_message_structs(&mut self, structs: Vec<TokenStream>) {
        self.message_structs.extend(structs);
    }

    fn push_method(&mut self, tokens: TokenStream) {
        self.methods.push(tokens);
    }

    fn into_artifact(self) -> InterfaceArtifact {
        let SubscribedServiceNode {
            node_name,
            message_structs,
            methods,
        } = self;

        let struct_ident = Ident::new("Subscribes", Span::call_site());

        let tokens: TokenStream = quote! {
            #( #message_structs )*

            pub struct #struct_ident;

            impl #struct_ident {
                #( #methods )*
            }
        };

        let rendered = render_tokens(tokens);

        InterfaceArtifact::from_kind(&node_name, InterfaceKind::SubscribedService, rendered)
    }
}

impl SubscribedTopicNode {
    fn new(node_name: String) -> Self {
        Self {
            node_name,
            message_structs: Vec::new(),
            methods: Vec::new(),
        }
    }

    fn extend_message_structs(&mut self, structs: Vec<TokenStream>) {
        self.message_structs.extend(structs);
    }

    fn push_method(&mut self, tokens: TokenStream) {
        self.methods.push(tokens);
    }

    fn into_artifact(self) -> InterfaceArtifact {
        let SubscribedTopicNode {
            node_name,
            message_structs,
            methods,
        } = self;

        let final_struct_ident = Ident::new("Subscribes", Span::call_site());
        let struct_tokens = build_subscribed_topic_struct(&final_struct_ident);

        let tokens: TokenStream = quote! {
            #( #message_structs )*

            #struct_tokens

            impl #final_struct_ident {
                #( #methods )*
            }
        };

        let rendered = render_tokens(tokens);

        InterfaceArtifact::from_kind(&node_name, InterfaceKind::SubscribedTopic, rendered)
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
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

    fn into_tokens(self) -> Vec<TokenStream> {
        let mut items: Vec<TokenStream> = self
            .structs
            .into_iter()
            .map(StructDefinition::into_tokens)
            .collect();
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
                quote!(#name: #field_ty)
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

fn collect_function_params(
    accept_format_artifacts: Option<&CapnpSchemaArtifacts>,
    return_format_artifacts: Option<&CapnpSchemaArtifacts>,
    struct_prefix: &str,
    context: &mut GenerationContext,
) -> Result<Vec<FunctionParam>> {
    if let Some(return_artifacts) = return_format_artifacts {
        let response_prefix = format!("{struct_prefix}Response");
        let response_ident = Ident::new(&response_prefix, Span::call_site());
        let mut fields = Vec::new();
        let mut ctor_params: Vec<TokenStream> = Vec::new();
        let mut ctor_bindings: Vec<TokenStream> = Vec::new();

        for (field_name, schema) in &return_artifacts.message_format().0 {
            let field_ident =
                Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
            let field_ty = schema_type_to_tokens(schema, &response_prefix, field_name, context);
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
    let capnp_params = artifacts
        .build_function_params()
        .map_err(Error::MessageEncoding)?;

    let mut schema_lookup: HashMap<String, (&String, &SchemaType)> =
        HashMap::with_capacity(format.0.len() * 2);
    for (name, schema) in &format.0 {
        let capnp_key = sanitize_capnp_field_name(name);
        schema_lookup.insert(capnp_key.clone(), (name, schema));

        let rust_key = sanitize_component(name);
        schema_lookup.entry(rust_key).or_insert((name, schema));
    }

    let mut params = Vec::with_capacity(capnp_params.len());
    for param in capnp_params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup
            .get(&key)
            .unwrap_or_else(|| panic!("missing schema entry for field `{key}`"));

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
    match schema {
        SchemaType::Type(token) => primitive_type_token(token),
        SchemaType::Array(array) => {
            let item_ty = match array.items.as_ref() {
                SchemaType::Type(token) => primitive_type_token(token),
                other => panic!(
                    "unsupported nested schema type {:?} in array `{field_name}`",
                    other
                ),
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
    let method_component = sanitize_component(field_name);
    let set_method = Ident::new(&format!("set_{method_component}"), Span::call_site());
    let init_method = Ident::new(&format!("init_{method_component}"), Span::call_site());

    match schema {
        SchemaType::Type(token) => match token {
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
                quote!(#builder_expr.#set_method(#value_expr);)
            }
            TypeToken::String => {
                quote!(#builder_expr.#set_method(#value_expr.as_str());)
            }
            TypeToken::Bytes => {
                quote!(#builder_expr.#set_method(#value_expr.as_ref());)
            }
            TypeToken::Time => {
                generate_time_assignment(builder_expr, &init_method, value_expr, names)
            }
        },
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Type(TypeToken::U8) => {
                quote!(#builder_expr.#set_method(#value_expr.as_ref());)
            }
            SchemaType::Type(token) => generate_list_assignment(
                builder_expr,
                &init_method,
                value_expr,
                array.length,
                token,
                names,
            ),
            other => panic!(
                "unsupported nested schema type {:?} in array `{field_name}`",
                other
            ),
        },
        SchemaType::Object(object) => generate_object_assignment(
            builder_expr,
            &init_method,
            value_expr,
            &object.fields,
            names,
        ),
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
    fields: &BTreeMap<String, SchemaType>,
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

fn build_topic_struct(struct_ident: &Ident) -> TokenStream {
    quote! {
        pub struct #struct_ident;
    }
}

fn build_service_struct(struct_ident: &Ident) -> TokenStream {
    quote! {
        pub struct #struct_ident;
    }
}

fn build_topic_emit(
    method_ident: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    topic: &ExposedTopic,
    label: &str,
) -> TokenStream {
    let param_tokens = function_param_tokens(params);
    let method_signature = if param_tokens.is_empty() {
        quote!(messenger: &crate::Messenger)
    } else {
        quote!(messenger: &crate::Messenger, #(#param_tokens),*)
    };
    let topic_literal = Literal::string(topic.name.as_str());
    let qos_tokens = qos_profile_tokens(&topic.qos_profile);
    let label_literal = Literal::string(label);

    match encoding {
        Some(spec) => {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let root_ident = Ident::new("root", Span::call_site());

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn #method_ident(#method_signature) -> crate::Result<()> {
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
                        messenger.namespace(),
                        topic_name,
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

fn build_subscribed_topic_struct(struct_ident: &Ident) -> TokenStream {
    quote! {
        pub struct #struct_ident;
    }
}

fn build_subscribed_topic_callback(
    fn_name: &Ident,
    args_struct_ident: &Ident,
    params: &[FunctionParam],
    artifacts: &CapnpSchemaArtifacts,
    encoding: &MessageEncodingSpec,
    topic: &SubscribedTopic,
    struct_prefix: &str,
) -> TokenStream {
    let topic_literal = Literal::string(topic.name.as_str());
    let reader_type = &encoding.reader_type;
    let helper_fn_ident = {
        let node_component = topic
            .node
            .as_deref()
            .map(sanitize_component)
            .unwrap_or_default();
        let topic_component = sanitize_component(topic.name.as_str());
        let helper_name = match (node_component.is_empty(), topic_component.is_empty()) {
            (true, true) => "deseralize_payload".to_string(),
            (true, false) => format!("deseralize_{}_payload", topic_component),
            (false, true) => format!("deseralize_{}_payload", node_component),
            (false, false) => format!("deseralize_{}_{}_payload", node_component, topic_component),
        };
        Ident::new(&helper_name, Span::call_site())
    };

    let mut schema_lookup: HashMap<String, (&String, &SchemaType)> = HashMap::new();
    for (field_name, schema) in &artifacts.message_format().0 {
        let capnp_key = sanitize_capnp_field_name(field_name);
        schema_lookup.insert(capnp_key.clone(), (field_name, schema));

        let rust_key = sanitize_component(field_name);
        schema_lookup
            .entry(rust_key)
            .or_insert((field_name, schema));
    }

    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();
    for param in params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup
            .get(&key)
            .unwrap_or_else(|| panic!("missing schema entry for field `{key}`"));
        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            original_name.as_str(),
            schema,
            struct_prefix,
            &mut names,
        );
        field_statements.append(&mut statements);
        let field_ident = &param.ident;
        field_inits.push(quote!(#field_ident: #value_ident));
    }

    let context_literal = Literal::string(struct_prefix);

    quote! {
        pub async fn #fn_name(messenger: &crate::Messenger) -> crate::Result<#args_struct_ident> {
            let topic_name = #topic_literal;
            let namespace = messenger.namespace();
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let mut subscription = peppylib::TopicMessenger::subscribe(
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

            Self::#helper_fn_ident(message.payload.as_ref())
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
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    match schema {
        SchemaType::Type(token) => {
            generate_primitive_reader(reader_expr, field_name, token, struct_prefix, names)
        }
        SchemaType::Array(array) => {
            generate_array_reader(reader_expr, field_name, array, struct_prefix, names)
        }
        SchemaType::Object(object) => generate_object_reader(
            reader_expr,
            field_name,
            &object.fields,
            struct_prefix,
            names,
        ),
    }
}

fn generate_primitive_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    token: &TypeToken,
    context: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context);

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
    context: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    match array.items.as_ref() {
        SchemaType::Type(TypeToken::U8) => generate_u8_array_reader(
            reader_expr,
            field_name,
            &method_ident,
            array.length,
            context,
            names,
        ),
        SchemaType::Type(token) => generate_primitive_array_reader(
            reader_expr,
            field_name,
            token,
            &method_ident,
            array.length,
            context,
            names,
        ),
        other => panic!(
            "unsupported nested schema type {:?} in array `{field_name}`",
            other
        ),
    }
}

fn generate_u8_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    method_ident: &Ident,
    length: Option<usize>,
    context: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("data");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context);

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
    context: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("list");
    let vec_ident = names.next("values");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(context);

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

    statements.push(quote! {
        let mut #vec_ident = Vec::with_capacity(#reader_ident.len() as usize);
        for value in #reader_ident.iter() {
            #vec_ident.push(value);
        }
    });

    match length {
        Some(len) => {
            let len_lit = Literal::usize_unsuffixed(len);
            statements.push(quote! {
                if #vec_ident.len() != #len_lit {
                    let actual = #vec_ident.len();
                    return Err(crate::Error::InvalidFixedListLength {
                        field: String::from(#field_literal),
                        expected: #len_lit,
                        actual,
                    });
                }
            });
            statements.push(quote! {
                let mut #value_ident: [#element_ty; #len_lit] = [#element_ty::default(); #len_lit];
                for (idx, element) in #vec_ident.into_iter().enumerate() {
                    #value_ident[idx] = element;
                }
            });
        }
        None => {
            statements.push(quote! {
                let #value_ident = #vec_ident;
            });
        }
    }

    (statements, value_ident)
}

fn generate_object_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    object: &BTreeMap<String, SchemaType>,
    struct_prefix: &str,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let reader_ident = names.next("reader");
    let field_literal = Literal::string(field_name);
    let context_literal = Literal::string(struct_prefix);
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
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
    service_name_literal: &Literal,
    request_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
) -> (TokenStream, Vec<TokenStream>) {
    let label_literal = Literal::string(label);
    let handler_fn_name = Ident::new(
        &format!("handle_{}_next_request", fn_name),
        Span::call_site(),
    );

    // Build callback parameter signature (just types, no names for Fn trait bounds)
    let callback_param_types: Vec<TokenStream> = if let Some(request_struct) = request_struct {
        vec![quote!(#request_struct)]
    } else {
        params.iter().map(|p| p.ty.clone()).collect()
    };

    // Determine response type
    let response_ty = response_spec
        .as_ref()
        .map(|spec| {
            let struct_ident = &spec.struct_ident;
            quote!(#struct_ident)
        })
        .unwrap_or_else(|| quote!(()));

    let method = match encoding {
        Some(request_spec) => {
            let service_name = service_name_literal;

            // Build request deserializer helper function
            let request_deserializer =
                build_request_deserializer(fn_name, request_spec, params, label, request_struct);

            let request_deserializer_name = Ident::new(
                &format!("{}_deserialize_request", fn_name),
                Span::call_site(),
            );

            // Build response serialization code inline
            let response_serialization = build_response_serialization_code(response_spec, label);

            // Build tuple pattern for deserialized params
            let param_idents: Vec<&Ident> = params.iter().map(|p| &p.ident).collect();
            let (params_pattern, callback_call) = if request_struct.is_some() {
                let binding_ident = Ident::new("request_data", Span::call_site());
                (quote!(#binding_ident), quote!(handler(#binding_ident)))
            } else if param_idents.is_empty() {
                (quote!(()), quote!(handler()))
            } else if param_idents.len() == 1 {
                let ident = param_idents[0];
                (quote!((#ident,)), quote!(handler(#ident)))
            } else {
                (
                    quote!((#(#param_idents),*)),
                    quote!(handler(#(#param_idents),*)),
                )
            };

            quote! {
                pub async fn #handler_fn_name<F>(
                    messenger: &crate::Messenger,
                    handler: F,
                ) -> crate::Result<()>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let namespace = messenger.namespace();
                    let service_name = #service_name;

                    let mut service = peppylib::ServiceMessenger::listen(
                        messenger.handle(),
                        namespace,
                        service_name,
                    )
                    .await?;

                    let handler_fn = handler;
                    let _handled_request = service
                        .handle_next_request(move |request_context| {
                            let handler = handler_fn;
                            async move {
                                let payload = request_context.message.payload;

                                let handler_result = (|| -> crate::Result<bytes::Bytes> {
                                    // Deserialize incoming request
                                    let #params_pattern = Self::#request_deserializer_name(payload.as_ref())?;

                                    // Call user handler
                                    let _response = #callback_call?;

                                    // Serialize response
                                    #response_serialization

                                    Ok(response_payload)
                                })();

                                handler_result.map_err(|error| {
                                    peppylib::PeppyError::Io(std::io::Error::other(
                                        error.to_string(),
                                    ))
                                })
                            }
                        })
                        .await?;

                    Ok(())
                }

                #request_deserializer
            }
        }
        None => {
            quote! {
                pub async fn #handler_fn_name<F>(
                    messenger: &crate::Messenger,
                    handler: F,
                ) -> crate::Result<()>
                where
                    F: Fn() -> crate::Result<#response_ty>,
                {
                    let _ = messenger;
                    let _ = handler;
                    Err(crate::Error::MessageFormatUnavailable {
                        context: String::from(#label_literal),
                    })
                }
            }
        }
    };

    (method, vec![])
}

fn build_request_struct(
    struct_prefix: &str,
    params: &[FunctionParam],
) -> Option<(Ident, TokenStream)> {
    if params.is_empty() {
        return None;
    }

    let ident = Ident::new(&format!("{struct_prefix}Request"), Span::call_site());
    let field_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(pub #ident: #ty)
        })
        .collect();

    let tokens = quote! {
        #[derive(Debug, Clone)]
        pub struct #ident {
            #( #field_tokens ),*
        }
    };

    Some((ident, tokens))
}

fn build_request_deserializer(
    fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    params: &[FunctionParam],
    label: &str,
    request_struct: Option<&Ident>,
) -> TokenStream {
    let deserializer_fn_name = Ident::new(
        &format!("{}_deserialize_request", fn_name),
        Span::call_site(),
    );
    let reader_type = &request_spec.reader_type;
    let context_literal = Literal::string(label);

    // Build return type as tuple or struct if available
    let return_ty = if let Some(request_struct) = request_struct {
        quote!(#request_struct)
    } else if params.is_empty() {
        quote!(())
    } else if params.len() == 1 {
        let ty = &params[0].ty;
        quote!((#ty,))
    } else {
        let types: Vec<&TokenStream> = params.iter().map(|p| &p.ty).collect();
        quote!((#(#types),*))
    };

    // Generate field deserialization (reuse existing field reader logic)
    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut value_idents = Vec::new();

    for param in params {
        let field_name = param.ident.to_string();
        // We need to find the schema for this field from the message format
        // For now, assume the field exists and generate basic reader code
        let method_ident = Ident::new(&format!("get_{}", &field_name), Span::call_site());
        let value_ident = names.next(&field_name);

        // Simple primitive reader for now - this should be enhanced to handle all types
        field_statements.push(quote! {
            let #value_ident = root.reborrow().#method_ident();
        });
        value_idents.push(value_ident);
    }

    let return_expr = if let Some(request_struct) = request_struct {
        let field_assignments: Vec<TokenStream> = params
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

            Ok(#return_expr)
        }
    }
}

fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    label: &str,
) -> TokenStream {
    let Some(_spec) = response_spec else {
        // No response, return empty bytes
        return quote! {
            let response_payload = bytes::Bytes::new();
        };
    };

    // TODO: Implement proper response serialization using the response spec format
    // For now, return empty bytes as placeholder
    let _context_literal = Literal::string(label);
    quote! {
        let response_payload = bytes::Bytes::new();
    }
}

fn build_sync_function(
    fn_name: &Ident,
    async_fn_name: &Ident,
    params: &[FunctionParam],
    label: &str,
) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();
    let label_literal = Literal::string(label);

    let async_args: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            quote!(#ident)
        })
        .collect();

    quote! {
        #[allow(clippy::too_many_arguments)]
        pub fn #fn_name(#(#param_tokens),*) -> crate::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|source| crate::Error::RuntimeInitialization {
                    context: String::from(#label_literal),
                    source,
                })?;
            runtime.block_on(#async_fn_name(#(#async_args),*))
        }
    }
}

fn build_async_function(
    fn_name: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();
    let label_literal = Literal::string(label);

    match encoding {
        Some(spec) => {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let root_ident = Ident::new("root", Span::call_site());

            quote! {
                pub async fn #fn_name(#(#param_tokens),*) -> crate::Result<()> {
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

                    let _ns = "temporary_namespace";
                    let _topic_name = "temporary_topic";
                    let _messenger = peppylib::MessengerHandle::new().await?;

                    // TODO: send_topic_message only applies to topics, `send_service_message` for services and `send_action_message` for actions
                    // messenger.send_topic_message(ns, topic_name, qos, message_payload)?;
                    Ok(())
                }
            }
        }
        None => {
            let suppress_unused = unused_params_stmt(params);
            let msg = Literal::string(&format!(
                "message format for `{label}` is not available in the generator"
            ));

            quote! {
                pub async fn #fn_name(#(#param_tokens),*) -> crate::Result<()> {
                    #suppress_unused
                    todo!(#msg);
                }
            }
        }
    }
}

fn build_async_returning_function(
    fn_name: &Ident,
    arguments: Option<&MessageFormat>,
    struct_suffix: &str,
    todo_msg: &str,
) -> Result<(Vec<TokenStream>, TokenStream)> {
    let fn_name_str = fn_name.to_string();
    let struct_prefix = to_camel_case(&fn_name_str);
    let mut context = GenerationContext::default();
    let format_artifacts = map_message_format(arguments)?;
    let params = collect_function_params(
        format_artifacts.as_ref(),
        None,
        &struct_prefix,
        &mut context,
    )?;

    let struct_name = if struct_suffix.is_empty() {
        struct_prefix
    } else {
        format!("{struct_prefix}{struct_suffix}")
    };
    let struct_ident = Ident::new(&struct_name, Span::call_site());

    let fields: Vec<(Ident, TokenStream)> = params
        .iter()
        .map(|param| (param.ident.clone(), param.ty.clone()))
        .collect();
    context.add_struct(struct_ident.clone(), fields);

    let struct_tokens = context.into_tokens();
    let todo_literal = Literal::string(todo_msg);

    let function_token = quote! {
        pub async fn #fn_name() -> #struct_ident {
            todo!(#todo_literal);
        }
    };

    Ok((struct_tokens, function_token))
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
