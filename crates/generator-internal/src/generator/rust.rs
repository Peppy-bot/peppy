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

    /// Builds the fire_goal method for subscribed actions using ActionMessenger::send_goal
    fn build_subscribed_action_fire_goal_method(
        &mut self,
        context: &mut GenerationContext,
        action_struct_name: &str,
        request_format: Option<&MessageFormat>,
        response_format: Option<&MessageFormat>,
        schema_key: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let request_artifacts = map_message_format(request_format)?;
        let response_artifacts = map_message_format(response_format)?;

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
        let goal_response_ident = Ident::new("GoalResponse", Span::call_site());

        let mut helper_items = Vec::new();
        let method_tokens = if let Some(ref response_artifacts) = response_artifacts {
            // Note: collect_function_params creates the GoalResponseData struct for us
            let _response_params = collect_function_params(
                None,
                Some(response_artifacts),
                &format!("{action_struct_name}GoalResponse"),
                context,
                Some(&goal_response_data_ident),
            )?;

            let goal_response_fields = vec![
                (
                    Ident::new("action_handle", Span::call_site()),
                    quote!(peppylib::messaging::ActionGoalHandle),
                ),
                (
                    Ident::new("data", Span::call_site()),
                    quote!(#goal_response_data_ident),
                ),
            ];
            context.add_struct_without_clone(goal_response_ident.clone(), goal_response_fields);

            // Build deserializer helper
            let response_schema_key = format!("{schema_key}_response");
            let response_schema = self.register_schema(
                &response_schema_key,
                "GoalResponseMessage",
                response_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();

            let response_format = response_artifacts.message_format();
            let mut response_statements = Vec::new();
            let mut response_inits = Vec::new();
            let mut names = NameGenerator::new();

            let context_expr = quote!(format!(
                "{} {} GoalResponse",
                TARGET_NODE_NAME, TARGET_ACTION_NAME
            ));

            for (field_name, schema) in &response_format.0 {
                let (mut statements, value_ident) = generate_field_reader_statements(
                    &quote!(root),
                    field_name,
                    schema,
                    "GoalResponseData",
                    &context_expr,
                    &mut names,
                );
                response_statements.append(&mut statements);
                let field_ident =
                    Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                response_inits.push(quote!(#field_ident: #value_ident));
            }

            let deserialize_helper = quote! {
                fn deserialize_goal_response(payload: &[u8]) -> crate::Result<#goal_response_data_ident> {
                    let context = format!("{} {} GoalResponse", TARGET_NODE_NAME, TARGET_ACTION_NAME);
                    let mut cursor = std::io::Cursor::new(payload);
                    let message_reader = capnp::serialize::read_message(
                        &mut cursor,
                        capnp::message::ReaderOptions::new(),
                    )
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: context.clone(),
                        source,
                    })?;

                    let root = message_reader
                        .get_root::<#reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: context.clone(),
                            source,
                        })?;

                    #( #response_statements )*

                    Ok(#goal_response_data_ident {
                        #( #response_inits ),*
                    })
                }
            };
            helper_items.push(deserialize_helper);

            // Build goal payload encoding
            let goal_payload_tokens = if let Some(ref request_artifacts) = request_artifacts {
                let schema_info = self.register_schema(
                    schema_key,
                    &format!("{action_struct_name}GoalMessage"),
                    request_artifacts,
                )?;
                let builder_type = schema_info.builder_type_tokens();

                // Generate assignments that read from request struct
                let request_ident = Ident::new("request", Span::call_site());
                let assignments = generate_assignments_from_struct(
                    &Ident::new("root", Span::call_site()),
                    request_format.expect("request_format should be Some when artifacts exist"),
                    &request_ident,
                );

                quote! {
                    let goal_payload = {
                        let mut message = capnp::message::Builder::new_default();
                        {
                            let mut root = message.init_root::<#builder_type>();
                            #( #assignments )*
                        }
                        let mut buffer = Vec::new();
                        capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                            crate::Error::CapnpSerialize {
                                context: format!("fire_goal {} {}", TARGET_NODE_NAME, TARGET_ACTION_NAME),
                                source,
                            }
                        })?;
                        bytes::Bytes::from(buffer)
                    };
                }
            } else {
                quote! {
                    let goal_payload = bytes::Bytes::new();
                }
            };

            let request_param = if request_format.is_some() {
                quote!(request: #goal_request_ident,)
            } else {
                quote!()
            };

            quote! {
                pub async fn fire_goal(
                    messenger: &crate::Messenger,
                    timeout: std::time::Duration,
                    target_master_node: Option<&str>,
                    target_instance_id: Option<&str>,
                    #request_param
                    feedback_qos: peppylib::config::QoSProfile,
                ) -> crate::Result<#goal_response_ident> {
                    #goal_payload_tokens

                    let action_handle = peppylib::ActionMessenger::send_goal(
                        messenger.handle(),
                        messenger.runtime().bound_master_node(),
                        messenger.runtime().bound_instance_id(),
                        TARGET_NODE_NAME,
                        TARGET_ACTION_NAME,
                        target_master_node,
                        target_instance_id,
                        goal_payload,
                        feedback_qos,
                        timeout,
                    )
                    .await?;

                    let payload = action_handle.goal_response().payload().as_bytes();
                    let response_data = deserialize_goal_response(payload.as_ref())?;
                    Ok(#goal_response_ident {
                        action_handle,
                        data: response_data,
                    })
                }
            }
        } else {
            let goal_response_fields = vec![(
                Ident::new("action_handle", Span::call_site()),
                quote!(peppylib::messaging::ActionGoalHandle),
            )];
            context.add_struct_without_clone(goal_response_ident.clone(), goal_response_fields);

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
                );

                quote! {
                    let goal_payload = {
                        let mut message = capnp::message::Builder::new_default();
                        {
                            let mut root = message.init_root::<#builder_type>();
                            #( #assignments )*
                        }
                        let mut buffer = Vec::new();
                        capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                            crate::Error::CapnpSerialize {
                                context: format!("fire_goal {} {}", TARGET_NODE_NAME, TARGET_ACTION_NAME),
                                source,
                            }
                        })?;
                        bytes::Bytes::from(buffer)
                    };
                }
            } else {
                quote! {
                    let goal_payload = bytes::Bytes::new();
                }
            };

            let request_param = if request_format.is_some() {
                quote!(request: #goal_request_ident,)
            } else {
                quote!()
            };

            quote! {
                pub async fn fire_goal(
                    messenger: &crate::Messenger,
                    timeout: std::time::Duration,
                    target_master_node: Option<&str>,
                    target_instance_id: Option<&str>,
                    #request_param
                    feedback_qos: peppylib::config::QoSProfile,
                ) -> crate::Result<#goal_response_ident> {
                    #goal_payload_tokens

                    let action_handle = peppylib::ActionMessenger::send_goal(
                        messenger.handle(),
                        messenger.runtime().bound_master_node(),
                        messenger.runtime().bound_instance_id(),
                        TARGET_NODE_NAME,
                        TARGET_ACTION_NAME,
                        target_master_node,
                        target_instance_id,
                        goal_payload,
                        feedback_qos,
                        timeout,
                    )
                    .await?;

                    Ok(#goal_response_ident {
                        action_handle,
                    })
                }
            }
        };

        Ok((method_tokens, helper_items))
    }

    /// Builds the cancel_goal method for subscribed actions using ActionMessenger::cancel_goal
    fn build_subscribed_action_cancel_method(
        &mut self,
        context: &mut GenerationContext,
        action_struct_name: &str,
        schema_key: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let cancel_response_format = cancel_action_response_format();
        let response_artifacts = map_message_format(Some(&cancel_response_format))?
            .expect("cancel response format should yield artifacts");

        let cancel_response_data_ident = Ident::new("CancelResponseData", Span::call_site());
        let cancel_response_ident = Ident::new("CancelResponse", Span::call_site());

        // Note: collect_function_params creates the CancelResponseData struct for us
        let _response_params = collect_function_params(
            None,
            Some(&response_artifacts),
            &format!("{action_struct_name}CancelResponse"),
            context,
            Some(&cancel_response_data_ident),
        )?;

        // CancelResponse wrapper struct
        let cancel_response_fields = vec![
            (Ident::new("master_node", Span::call_site()), quote!(String)),
            (Ident::new("instance_id", Span::call_site()), quote!(String)),
            (
                Ident::new("data", Span::call_site()),
                quote!(#cancel_response_data_ident),
            ),
        ];
        context.add_struct(cancel_response_ident.clone(), cancel_response_fields);

        // Build deserializer helper
        let response_schema_key = format!("{schema_key}_response");
        let response_schema = self.register_schema(
            &response_schema_key,
            "CancelResponseMessage",
            &response_artifacts,
        )?;
        let reader_type = response_schema.reader_type_tokens();

        let response_format = response_artifacts.message_format();
        let mut response_statements = Vec::new();
        let mut response_inits = Vec::new();
        let mut names = NameGenerator::new();

        let context_expr = quote!(format!(
            "{} {} CancelResponse",
            TARGET_NODE_NAME, TARGET_ACTION_NAME
        ));

        for (field_name, schema) in &response_format.0 {
            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                field_name,
                schema,
                "CancelResponseData",
                &context_expr,
                &mut names,
            );
            response_statements.append(&mut statements);
            let field_ident =
                Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
            response_inits.push(quote!(#field_ident: #value_ident));
        }

        let deserialize_helper = quote! {
            fn deserialize_cancel_response(payload: &[u8]) -> crate::Result<#cancel_response_data_ident> {
                let context = format!("{} {} CancelResponse", TARGET_NODE_NAME, TARGET_ACTION_NAME);
                let mut cursor = std::io::Cursor::new(payload);
                let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: context.clone(),
                    source,
                })?;

                let root = message_reader
                    .get_root::<#reader_type>()
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: context.clone(),
                        source,
                    })?;

                #( #response_statements )*

                Ok(#cancel_response_data_ident {
                    #( #response_inits ),*
                })
            }
        };

        let method_tokens = quote! {
            pub async fn cancel_goal(
                messenger: &crate::Messenger,
                action_handle: &peppylib::messaging::ActionGoalHandle,
                timeout: std::time::Duration,
            ) -> crate::Result<#cancel_response_ident> {
                let response = peppylib::ActionMessenger::cancel_goal(
                    messenger.handle(),
                    action_handle,
                    timeout,
                )
                .await?;

                let payload = response.payload().as_bytes();
                let response_data = deserialize_cancel_response(payload.as_ref())?;
                Ok(#cancel_response_ident {
                    master_node: response.master_node().to_string(),
                    instance_id: response.instance_id().to_string(),
                    data: response_data,
                })
            }
        };

        Ok((method_tokens, vec![deserialize_helper]))
    }

    /// Builds the on_next_feedback_message method for subscribed actions
    fn build_subscribed_action_feedback_method(
        &mut self,
        context: &mut GenerationContext,
        format: &MessageFormat,
        action_struct_name: &str,
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

        let helper_fn_ident = Ident::new("deserialize_feedback_payload", Span::call_site());

        let format_schema = format_artifacts.message_format();
        let schema_lookup = SchemaFieldLookup::new(format_schema);

        let context_expr = quote!(format!(
            "{} {} FeedbackMessage",
            TARGET_NODE_NAME, TARGET_ACTION_NAME
        ));

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
                &context_expr,
                &mut names,
            );
            field_statements.append(&mut statements);
            let field_ident = &param.ident;
            field_inits.push(quote!(#field_ident: #value_ident));
        }

        let helper_tokens = quote! {
            fn #helper_fn_ident(payload: &[u8]) -> crate::Result<#struct_ident> {
                let context = format!("{} {} FeedbackMessage", TARGET_NODE_NAME, TARGET_ACTION_NAME);
                let mut cursor = std::io::Cursor::new(payload);
                let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: context.clone(),
                    source,
                })?;

                let root = message_reader
                    .get_root::<#reader_type>()
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: context.clone(),
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
                action_handle: &mut peppylib::messaging::ActionGoalHandle,
            ) -> crate::Result<#struct_ident> {
                let feedback = action_handle.on_next_feedback().await?;
                let payload = feedback.payload().as_bytes();
                #helper_fn_ident(payload.as_ref())
            }
        };

        Ok((method_tokens, vec![helper_tokens]))
    }

    /// Builds the get_result method for subscribed actions using ActionMessenger::request_result
    fn build_subscribed_action_result_method(
        &mut self,
        context: &mut GenerationContext,
        action_struct_name: &str,
        response_format: Option<&MessageFormat>,
        schema_key: &str,
    ) -> Result<(TokenStream, Vec<TokenStream>)> {
        let response_artifacts = map_message_format(response_format)?;

        let result_response_data_ident = Ident::new("ResultResponseData", Span::call_site());
        let result_response_ident = Ident::new("ResultResponse", Span::call_site());

        let mut helper_items = Vec::new();
        let method_tokens = if let Some(ref response_artifacts) = response_artifacts {
            // Note: collect_function_params creates the ResultResponseData struct for us
            let _response_params = collect_function_params(
                None,
                Some(response_artifacts),
                &format!("{action_struct_name}ResultResponse"),
                context,
                Some(&result_response_data_ident),
            )?;

            // ResultResponse wrapper
            let result_response_fields = vec![
                (Ident::new("master_node", Span::call_site()), quote!(String)),
                (Ident::new("instance_id", Span::call_site()), quote!(String)),
                (
                    Ident::new("data", Span::call_site()),
                    quote!(#result_response_data_ident),
                ),
            ];
            context.add_struct(result_response_ident.clone(), result_response_fields);

            // Build deserializer helper
            let response_schema_key = format!("{schema_key}_response");
            let response_schema = self.register_schema(
                &response_schema_key,
                "ResultResponseMessage",
                response_artifacts,
            )?;
            let reader_type = response_schema.reader_type_tokens();

            let response_format = response_artifacts.message_format();
            let mut response_statements = Vec::new();
            let mut response_inits = Vec::new();
            let mut names = NameGenerator::new();

            let context_expr = quote!(format!(
                "{} {} ResultResponse",
                TARGET_NODE_NAME, TARGET_ACTION_NAME
            ));

            for (field_name, schema) in &response_format.0 {
                let (mut statements, value_ident) = generate_field_reader_statements(
                    &quote!(root),
                    field_name,
                    schema,
                    "ResultResponseData",
                    &context_expr,
                    &mut names,
                );
                response_statements.append(&mut statements);
                let field_ident =
                    Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                response_inits.push(quote!(#field_ident: #value_ident));
            }

            let deserialize_helper = quote! {
                fn deserialize_result_response(payload: &[u8]) -> crate::Result<#result_response_data_ident> {
                    let context = format!("{} {} ResultResponse", TARGET_NODE_NAME, TARGET_ACTION_NAME);
                    let mut cursor = std::io::Cursor::new(payload);
                    let message_reader = capnp::serialize::read_message(
                        &mut cursor,
                        capnp::message::ReaderOptions::new(),
                    )
                    .map_err(|source| crate::Error::CapnpDeserialize {
                        context: context.clone(),
                        source,
                    })?;

                    let root = message_reader
                        .get_root::<#reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: context.clone(),
                            source,
                        })?;

                    #( #response_statements )*

                    Ok(#result_response_data_ident {
                        #( #response_inits ),*
                    })
                }
            };
            helper_items.push(deserialize_helper);

            quote! {
                pub async fn get_result(
                    messenger: &crate::Messenger,
                    action_handle: &peppylib::messaging::ActionGoalHandle,
                    timeout: std::time::Duration,
                ) -> crate::Result<#result_response_ident> {
                    let response = peppylib::ActionMessenger::request_result(
                        messenger.handle(),
                        action_handle,
                        timeout,
                    )
                    .await?;

                    let payload = response.payload().as_bytes();
                    let response_data = deserialize_result_response(payload.as_ref())?;
                    Ok(#result_response_ident {
                        master_node: response.master_node().to_string(),
                        instance_id: response.instance_id().to_string(),
                        data: response_data,
                    })
                }
            }
        } else {
            // No response format - return unit type wrapped
            let result_response_fields = vec![
                (Ident::new("master_node", Span::call_site()), quote!(String)),
                (Ident::new("instance_id", Span::call_site()), quote!(String)),
            ];
            context.add_struct(result_response_ident.clone(), result_response_fields);

            quote! {
                pub async fn get_result(
                    messenger: &crate::Messenger,
                    action_handle: &peppylib::messaging::ActionGoalHandle,
                    timeout: std::time::Duration,
                ) -> crate::Result<#result_response_ident> {
                    let response = peppylib::ActionMessenger::request_result(
                        messenger.handle(),
                        action_handle,
                        timeout,
                    )
                    .await?;

                    Ok(#result_response_ident {
                        master_node: response.master_node().to_string(),
                        instance_id: response.instance_id().to_string(),
                    })
                }
            }
        };

        Ok((method_tokens, helper_items))
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

        // Build RequestData struct (contains the payload fields)
        let request_data_struct_ident = if let Some((ident, tokens)) =
            build_request_struct_with_name_and_impl("RequestData", &handler_params, false)
        {
            context.add_private_struct(tokens);
            Some(ident)
        } else {
            None
        };

        // Build Request struct (wraps instance_id, master_node, and data)
        let request_struct_tokens = if let Some(ref data_ident) = request_data_struct_ident {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct Request {
                    pub instance_id: String,
                    pub master_node: String,
                    pub data: #data_ident,
                }
            }
        } else {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct Request {
                    pub instance_id: String,
                    pub master_node: String,
                }
            }
        };
        context.add_private_struct(request_struct_tokens);
        let request_struct_ident = Some(Ident::new("Request", Span::call_site()));

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
            request_data_struct_ident.as_ref(),
            response_spec.as_ref(),
            true, // use_service_name_const
        );

        // Add service_name constant at module level
        let service_name_const = {
            let service_name_str = Literal::string(service.name.as_str());
            quote!(const SERVICE_NAME: &str = #service_name_str;)
        };

        let mut service_tokens = vec![service_name_const];
        service_tokens.extend(context.into_tokens());
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
        let mut action_handle_methods: Vec<TokenStream> = Vec::new();
        let mut helper_tokens: Vec<TokenStream> = Vec::new();

        // ACTION_NAME constant
        let action_name_literal = Literal::string(&action.name);

        // Track whether we have each component
        let has_goal = action.goal_service.is_some();
        let has_feedback = action.feedback_topic.is_some();
        let has_result = action.result_service.is_some();

        // Generate goal service handler
        if let Some(goal) = action.goal_service.as_ref() {
            let label = format!("{base_name}_goal");
            let schema_struct_prefix = format!("{action_prefix}Goal");

            // Process request format
            let request_artifacts = map_message_format(goal.request_message_format.as_ref())?;
            let response_artifacts = map_message_format(goal.response_message_format.as_ref())?;

            let goal_data_params = collect_function_params(
                request_artifacts.as_ref(),
                response_artifacts.as_ref(),
                "Goal",
                &mut context,
                None,
            )?;

            // Build GoalRequestData struct if there are request params
            let goal_request_data_struct = if let Some((ident, tokens)) =
                build_request_struct_with_name_and_impl("GoalRequestData", &goal_data_params, false)
            {
                context.add_private_struct(tokens);
                Some(ident)
            } else {
                None
            };

            // Build GoalRequest struct wrapping instance_id, master_node, and data
            let goal_request_struct_tokens = if let Some(ref data_ident) = goal_request_data_struct
            {
                quote! {
                    #[derive(Debug, Clone)]
                    #[allow(dead_code)]
                    pub struct GoalRequest {
                        pub instance_id: String,
                        pub master_node: String,
                        pub data: #data_ident,
                    }
                }
            } else {
                quote! {
                    #[derive(Debug, Clone)]
                    #[allow(dead_code)]
                    pub struct GoalRequest {
                        pub instance_id: String,
                        pub master_node: String,
                    }
                }
            };
            context.add_private_struct(goal_request_struct_tokens);

            // Build encoding for request deserialization
            let encoding = self.prepare_message_encoding(
                &label,
                &schema_struct_prefix,
                request_artifacts.as_ref(),
                &goal_data_params,
            )?;

            // Build response serialization spec
            let response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
                let response_schema_prefix = format!("{schema_struct_prefix}Response");
                let schema_key = format!("{label}_response");
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

            // Build deserializer helper
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
                );
                helper_tokens.push(deserializer_fn);
            }

            // Build payload handler
            let goal_handler_fn = build_action_payload_handler(
                &Ident::new("handle_goal_payload", Span::call_site()),
                &Ident::new("deserialize_goal_request", Span::call_site()),
                &Ident::new("GoalRequest", Span::call_site()),
                goal_request_data_struct.as_ref(),
                response_spec.as_ref(),
                encoding.is_some(),
            );
            helper_tokens.push(goal_handler_fn);

            // Build handle_goal_next_request method
            let goal_method = build_action_handle_method(
                &Ident::new("handle_goal_next_request", Span::call_site()),
                &Ident::new("handle_goal_payload", Span::call_site()),
                &Ident::new("GoalRequest", Span::call_site()),
                &Ident::new("GoalResponse", Span::call_site()),
                &Ident::new("goal_service", Span::call_site()),
                encoding.is_some(),
            );
            action_handle_methods.push(goal_method);

            // Generate cancel handler
            let cancel_label = format!("{base_name}_goal_cancel");
            let cancel_schema_prefix = format!("{action_prefix}GoalCancel");
            let cancel_response_format = cancel_action_response_format();
            let cancel_response_artifacts = map_message_format(Some(&cancel_response_format))?;

            // CancelRequest struct
            context.add_private_struct(quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct CancelRequest {
                    pub instance_id: String,
                    pub master_node: String,
                }
            });

            // CancelResponse struct
            let _cancel_params = collect_function_params(
                None,
                cancel_response_artifacts.as_ref(),
                "Cancel",
                &mut context,
                None,
            )?;

            // Register cancel response schema
            let cancel_response_spec =
                if let Some(return_artifacts) = cancel_response_artifacts.as_ref() {
                    let schema_key = format!("{cancel_label}_response");
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

            // Build cancel payload handler
            let cancel_handler_fn = build_action_payload_handler(
                &Ident::new("handle_cancel_payload", Span::call_site()),
                &Ident::new("deserialize_cancel_request", Span::call_site()),
                &Ident::new("CancelRequest", Span::call_site()),
                None,
                cancel_response_spec.as_ref(),
                false,
            );
            helper_tokens.push(cancel_handler_fn);

            // Build handle_cancel_next_request method
            let cancel_method = build_action_handle_method(
                &Ident::new("handle_cancel_next_request", Span::call_site()),
                &Ident::new("handle_cancel_payload", Span::call_site()),
                &Ident::new("CancelRequest", Span::call_site()),
                &Ident::new("CancelResponse", Span::call_site()),
                &Ident::new("cancel_service", Span::call_site()),
                false,
            );
            action_handle_methods.push(cancel_method);
        }

        // Generate result service handler
        if let Some(result) = action.result_service.as_ref() {
            let label = format!("{base_name}_result");
            let schema_struct_prefix = format!("{action_prefix}Result");

            let response_artifacts = map_message_format(result.response_message_format.as_ref())?;

            // ResultRequest struct
            context.add_private_struct(quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct ResultRequest {
                    pub instance_id: String,
                    pub master_node: String,
                }
            });

            // ResultResponse struct
            let _result_params = collect_function_params(
                None,
                response_artifacts.as_ref(),
                "Result",
                &mut context,
                None,
            )?;

            // Register result response schema
            let result_response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
                let schema_key = format!("{label}_response");
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

            // Build result payload handler
            let result_handler_fn = build_action_payload_handler(
                &Ident::new("handle_result_payload", Span::call_site()),
                &Ident::new("deserialize_result_request", Span::call_site()),
                &Ident::new("ResultRequest", Span::call_site()),
                None,
                result_response_spec.as_ref(),
                false,
            );
            helper_tokens.push(result_handler_fn);

            // Build handle_result_next_request method
            let result_method = build_action_handle_method(
                &Ident::new("handle_result_next_request", Span::call_site()),
                &Ident::new("handle_result_payload", Span::call_site()),
                &Ident::new("ResultRequest", Span::call_site()),
                &Ident::new("ResultResponse", Span::call_site()),
                &Ident::new("result_service", Span::call_site()),
                false,
            );
            action_handle_methods.push(result_method);
        }

        // Generate feedback emitter
        if let Some(feedback) = action.feedback_topic.as_ref() {
            let label = format!("emit_feedback {}", &action.name);
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
                &format!("emit_{base_name}_feedback"),
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

        // Build ActionHandle struct and expose method
        let action_handle_struct = build_action_handle_struct(has_goal, has_feedback, has_result);
        let expose_method = build_action_expose_method();

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
        let response_data_ident = Ident::new("ResponseData", Span::call_site());

        let params = collect_function_params(
            request_artifacts.as_ref(),
            response_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
            Some(&response_data_ident),
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
                            context: format!("poll {} {}", NODE_NAME, SERVICE_NAME),
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
                messenger.runtime().bound_master_node(),
                messenger.runtime().bound_instance_id(),
                NODE_NAME,
                SERVICE_NAME,
                target_master_node,
                target_instance_id,
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
                let schema_lookup = SchemaFieldLookup::new(response_format);
                let mut response_statements = Vec::new();
                let mut response_inits = Vec::new();
                let mut name_gen = NameGenerator::new();
                let context_expr = quote!(context.clone());
                for (field_name, _) in &response_format.0 {
                    let (original_name, schema) = schema_lookup.get(field_name);
                    let (mut statements, value_ident) = generate_field_reader_statements(
                        &quote!(root),
                        original_name.as_str(),
                        schema,
                        &response_struct_name,
                        &context_expr,
                        &mut name_gen,
                    );
                    response_statements.append(&mut statements);
                    let field_ident =
                        Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
                    response_inits.push(quote!(#field_ident: #value_ident));
                }

                let poll_tokens = quote! {
                    let response_message = #poll_call.await?;
                };

                let response_struct_tokens = quote! {
                    #[derive(Debug, Clone)]
                    pub struct #response_struct_ident {
                        pub master_node: String,
                        pub instance_id: String,
                        pub data: #response_data_ident,
                    }
                };
                context.add_private_struct(response_struct_tokens);

                let response_tokens = quote! {
                    let payload = response_message.payload().as_bytes();
                    let response_data = deserialize_response(&payload)?;
                    Ok(#response_struct_ident {
                        master_node: response_message.master_node().to_string(),
                        instance_id: response_message.instance_id().to_string(),
                        data: response_data,
                    })
                };

                let deserialize_fn = quote! {
                    fn deserialize_response(payload: &[u8]) -> crate::Result<#response_data_ident> {
                        let context = format!("{} {} response", NODE_NAME, SERVICE_NAME);
                        let mut cursor = std::io::Cursor::new(payload);
                        let message_reader = capnp::serialize::read_message(
                            &mut cursor,
                            capnp::message::ReaderOptions::new(),
                        )
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: context.clone(),
                            source,
                        })?;

                        let root = message_reader
                            .get_root::<#response_reader_type>()
                            .map_err(|source| crate::Error::CapnpDeserialize {
                                context: context.clone(),
                                source,
                            })?;

                        #( #response_statements )*

                        Ok(#response_data_ident {
                            #( #response_inits ),*
                        })
                    }
                };

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
        let node_name_literal = Literal::string(service.node.as_str());

        // Add NODE_NAME and SERVICE_NAME constants at the beginning
        let constants_tokens = quote! {
            const NODE_NAME: &str = #node_name_literal;
            const SERVICE_NAME: &str = #service_name_literal;
        };

        let mut fn_param_tokens = vec![
            quote!(messenger: &crate::Messenger),
            quote!(timeout: std::time::Duration),
            quote!(target_master_node: Option<&str>),
            quote!(target_instance_id: Option<&str>),
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

        // Insert constants at the beginning, then service tokens, then function, then deserialize helper
        let mut all_tokens = vec![constants_tokens];
        all_tokens.append(&mut service_tokens);
        all_tokens.push(function_token);
        if let Some(deserialize_fn) = deserialize_fn_tokens {
            all_tokens.push(deserialize_fn);
        }

        let mut module_name = subscribed_service_module_name(service);
        if module_name.is_empty() {
            module_name = method_label
                .strip_prefix("poll_")
                .map(|label| label.to_string())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| method_label.clone());
        }

        let tokens: TokenStream = quote! {
            #( #all_tokens )*
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

        let mut context = GenerationContext::default();
        let mut methods: Vec<TokenStream> = Vec::new();
        let mut helper_items: Vec<TokenStream> = Vec::new();

        // Generate constants
        let node_name_literal = Literal::string(action.node.as_str());
        let action_name_literal = Literal::string(action.name.as_str());
        let constants_tokens = quote! {
            const TARGET_NODE_NAME: &str = #node_name_literal;
            const TARGET_ACTION_NAME: &str = #action_name_literal;
        };

        let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
        let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
        let feedback_format = non_empty_message_format(messages.feedback.as_ref());
        let result_response_format = non_empty_message_format(messages.result_response.as_ref());

        // fire_goal method
        let goal_schema_key = format!("{action_struct_name}_fire_goal");
        let (goal_method, mut goal_helpers) = self.build_subscribed_action_fire_goal_method(
            &mut context,
            &action_struct_name,
            goal_request_format,
            goal_response_format,
            &goal_schema_key,
        )?;
        methods.push(goal_method);
        helper_items.append(&mut goal_helpers);

        // cancel_goal method
        let cancel_schema_key = format!("{action_struct_name}_cancel_goal");
        let (cancel_method, mut cancel_helpers) = self.build_subscribed_action_cancel_method(
            &mut context,
            &action_struct_name,
            &cancel_schema_key,
        )?;
        methods.push(cancel_method);
        helper_items.append(&mut cancel_helpers);

        // on_next_feedback_message method (only if feedback format exists)
        if let Some(feedback_format) = feedback_format {
            let (feedback_method, mut feedback_helpers) = self
                .build_subscribed_action_feedback_method(
                    &mut context,
                    feedback_format,
                    &action_struct_name,
                )?;
            methods.push(feedback_method);
            helper_items.append(&mut feedback_helpers);
        }

        // get_result method
        let result_schema_key = format!("{action_struct_name}_get_result");
        let (result_method, mut result_helpers) = self.build_subscribed_action_result_method(
            &mut context,
            &action_struct_name,
            result_response_format,
            &result_schema_key,
        )?;
        methods.push(result_method);
        helper_items.append(&mut result_helpers);

        // Assemble all items
        let mut items = vec![constants_tokens];
        items.extend(context.into_tokens());
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
        self.add_struct_with_derives(ident, fields, true)
    }

    fn add_struct_without_clone(&mut self, ident: Ident, fields: Vec<(Ident, TokenStream)>) {
        self.add_struct_with_derives(ident, fields, false)
    }

    fn add_struct_with_derives(
        &mut self,
        ident: Ident,
        fields: Vec<(Ident, TokenStream)>,
        derive_clone: bool,
    ) {
        if let Some(existing) = self.structs.iter_mut().find(|def| def.ident == ident) {
            *existing = StructDefinition {
                ident,
                fields,
                derive_clone,
            };
        } else {
            self.structs.push(StructDefinition {
                ident,
                fields,
                derive_clone,
            });
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
    derive_clone: bool,
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

        let derive_attr = if self.derive_clone {
            quote!(#[derive(Debug, Clone)])
        } else {
            quote!(#[derive(Debug)])
        };

        if field_tokens.is_empty() {
            quote! {
                #derive_attr
                #[allow(dead_code)]
                pub struct #ident {}
            }
        } else {
            quote! {
                #derive_attr
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

/// Generates assignments from a struct access (e.g., request.field_name) instead of direct param access
fn generate_assignments_from_struct(
    builder_ident: &Ident,
    format: &MessageFormat,
    struct_ident: &Ident,
) -> Vec<TokenStream> {
    let mut assignments = Vec::with_capacity(format.0.len());
    let mut name_gen = NameGenerator::new();
    let builder_expr = quote!(#builder_ident);

    for (field_name, schema) in &format.0 {
        let field_ident = Ident::new(&sanitize_component(field_name), Span::call_site());
        let value_expr = quote!(#struct_ident.#field_ident);
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
                    let qos = #qos_tokens;
                    let as_topic = #topic_literal;
                    let as_node_name = messenger.runtime().node_name();
                    let as_instance_id = messenger.runtime().bound_instance_id();
                    let with_master_node = messenger.runtime().bound_master_node();

                    peppylib::TopicMessenger::emit(
                        messenger.handle(),
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
            messenger: &crate::Messenger,
            master_node_target: Option<&str>,
            instance_id_target: Option<&str>,
        ) -> crate::Result<(String, #args_struct_ident)> {
            let topic_name = #topic_literal;
            let node_name = #node_name_literal;
            let qos = peppylib::config::QoSProfile::Standard;

            let message = {
                let subscription_future = peppylib::TopicMessenger::subscribe(
                    messenger.handle(),
                    messenger.runtime().bound_master_node(),
                    messenger.runtime().bound_instance_id(),
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

fn generate_field_reader_statements(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    generate_field_reader_statements_inner(
        reader_expr,
        field_name,
        schema,
        struct_prefix,
        context_expr,
        names,
        true,
    )
}

fn generate_field_reader_statements_inner(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
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
                context_expr,
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
                context_expr,
                names,
                false,
            );
            statements.push(quote!(let #option_ident = Some(#value_ident);));
            return (statements, option_ident);
        }
    }

    match schema {
        SchemaType::Type(token) => {
            generate_primitive_reader(reader_expr, field_name, token, context_expr, names)
        }
        SchemaType::Primitive(primitive) => generate_primitive_reader(
            reader_expr,
            field_name,
            &primitive.kind,
            context_expr,
            names,
        ),
        SchemaType::Array(array) => {
            generate_array_reader(reader_expr, field_name, array, context_expr, names)
        }
        SchemaType::Object(object) => generate_object_reader(
            reader_expr,
            field_name,
            &object.fields,
            struct_prefix,
            context_expr,
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
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

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
                            context: #context_expr,
                            source,
                        })?;
                },
                quote! {
                    let #value_ident = #reader_ident
                        .to_str()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: #context_expr,
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
                            context: #context_expr,
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
                            context: #context_expr,
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
    context_expr: &TokenStream,
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
            context_expr,
            names,
        ),
        Some(token) => generate_primitive_array_reader(
            reader_expr,
            field_name,
            token,
            &method_ident,
            array.length,
            context_expr,
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
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("data");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
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
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("list");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

    let element_ty = primitive_type_token(token);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
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
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let reader_ident = names.next("reader");
    let field_literal = Literal::string(field_name);
    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
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
            context_expr,
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
    request_data_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
    use_service_name_const: bool,
) -> (TokenStream, Vec<TokenStream>) {
    let handler_fn_name = handler_fn_name_override.cloned().unwrap_or_else(|| {
        Ident::new(
            &format!("handle_{}_next_request", fn_name),
            Span::call_site(),
        )
    });

    // Build callback parameter signature (just types, no names for Fn trait bounds)
    let callback_param_types: Vec<TokenStream> = if use_service_name_const {
        // For exposed services: handler takes Request (which wraps instance_id, master_node, request_data)
        if let Some(request_struct) = request_struct {
            vec![quote!(#request_struct)]
        } else {
            Vec::new()
        }
    } else {
        // For action handlers: handler takes (instance_id, Request) or just the params
        let mut types = Vec::new();
        if let Some(instance_param) = instance_id_param {
            types.push(instance_param.ty.clone());
        }
        if let Some(request_struct) = request_struct {
            types.push(quote!(#request_struct));
        } else {
            types.extend(handler_params.iter().map(|p| p.ty.clone()));
        }
        types
    };

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

    let instance_from_request_context = instance_id_param.is_some();

    let instance_binding_ident =
        instance_id_param.map(|_| Ident::new("instance_id", Span::call_site()));

    let (request_pattern, callback_call): (TokenStream, TokenStream) = if use_service_name_const {
        let binding_ident = Ident::new("request_data", Span::call_site());
        let request_ident = Ident::new("request", Span::call_site());
        if request_data_struct.is_some() {
            (quote!(#binding_ident), quote!(handler(#request_ident)))
        } else {
            (quote!(()), quote!(handler(#request_ident)))
        }
    } else {
        let (pattern, handler_request_args): (TokenStream, Vec<TokenStream>) =
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
        let call = if let Some(instance_ident) = instance_binding_ident.as_ref() {
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
        (pattern, call)
    };

    let response_serialization = build_response_serialization_code(
        response_spec,
        label,
        &callback_call,
        service_instance_param_ident.as_ref(),
        use_service_name_const,
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

        let deserializer_struct = if use_service_name_const {
            request_data_struct
        } else {
            request_struct
        };

        let request_deserializer = if instance_from_request_context {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                deserializer_struct,
                None,
                use_service_name_const,
            )
        } else {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                deserializer_struct,
                instance_id_param,
                use_service_name_const,
            )
        };
        helper_tokens.push(request_deserializer);

        let deserializer_pattern = if use_service_name_const {
            request_pattern.clone()
        } else if let Some(instance_ident) = instance_binding_ident.as_ref() {
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

        if use_service_name_const {
            helper_params.push(quote!(master_node: String));
            helper_params.push(quote!(instance_id: String));
        } else if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = if use_service_name_const {
            let request_construction = if request_data_struct.is_some() {
                quote!(let request = Request { instance_id, master_node, data: request_data };)
            } else {
                quote!(let request = Request { instance_id, master_node };)
            };
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let #deserializer_pattern = #request_deserializer_name(payload)?;
                    #request_construction

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        } else {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let #deserializer_pattern = #request_deserializer_name(payload)?;

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        };
        helper_tokens.push(helper_fn);
    } else {
        let mut helper_params: Vec<TokenStream> = vec![quote!(handler: &F)];

        if use_service_name_const {
            helper_params.push(quote!(master_node: String));
            helper_params.push(quote!(instance_id: String));
        } else if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = if use_service_name_const {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let request = Request { instance_id, master_node };

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        } else {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        };
        helper_tokens.push(helper_fn);
    }

    let request_context_ident = if encoding.is_some() || instance_from_request_context {
        Ident::new("request_context", Span::call_site())
    } else {
        Ident::new("_request_context", Span::call_site())
    };

    let helper_call_tokens = if use_service_name_const {
        if encoding.is_some() {
            let mut helper_args: Vec<TokenStream> = vec![
                quote!(payload.as_ref()),
                quote!(&handler),
                quote!(master_node),
                quote!(instance_id),
            ];

            if let Some(arg) = service_instance_call_arg.clone() {
                helper_args.push(arg);
            }

            quote!({
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let master_node = message.master_node().to_string();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            let mut helper_args: Vec<TokenStream> =
                vec![quote!(&handler), quote!(master_node), quote!(instance_id)];

            if let Some(arg) = service_instance_call_arg.clone() {
                helper_args.push(arg);
            }

            quote!({
                let message = #request_context_ident.message();
                let master_node = message.master_node().to_string();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        }
    } else if encoding.is_some() {
        let mut helper_args: Vec<TokenStream> = vec![quote!(payload.as_ref()), quote!(&handler)];

        if instance_from_request_context {
            helper_args.push(quote!(instance_id));
        }

        if let Some(arg) = service_instance_call_arg.clone() {
            helper_args.push(arg);
        }

        if instance_from_request_context {
            quote!({
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let instance_id = message.instance_id().to_string();
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
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            quote!(#handler_helper_name(#(#helper_args),*))
        }
    };

    let service_name_ref = if use_service_name_const {
        quote!(SERVICE_NAME)
    } else {
        quote!(service_name)
    };

    let method = if use_service_name_const {
        quote! {
            pub async fn #handler_fn_name<F>(
                messenger: &crate::Messenger,
                handler: F,
            ) -> crate::Result<()>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                let mut service = peppylib::ServiceMessenger::listen(
                    messenger.handle(),
                    messenger.runtime().bound_master_node(),
                    messenger.runtime().bound_instance_id(),
                    messenger.runtime().node_name(),
                    #service_name_ref,
                )
                .await?;

                service
                    .handle_next_request(move |#request_context_ident| {
                        async move {
                            #helper_call_tokens.map_err(|error| {
                                peppylib::PeppyError::Io(std::io::Error::other(error.to_string()))
                            })
                        }
                    })
                    .await?;

                Ok(())
            }
        }
    } else {
        let env_var_literal = Literal::string("PEPPY_INSTANCE_ID");
        let service_instance_env_stmt = quote! {
            let service_instance_id = std::env::var(#env_var_literal).map_err(|source| {
                crate::Error::MissingInstanceIdEnvVar {
                    var: #env_var_literal,
                    source,
                }
            })?;
        };
        let service_instance_clone_stmt = if needs_service_instance_id {
            quote!(let service_instance_id = service_instance_id.clone();)
        } else {
            TokenStream::new()
        };
        let service_name_binding = quote!(let service_name = #service_name;);

        quote! {
            pub async fn #handler_fn_name<F>(
                messenger: &crate::Messenger,
                handler: F,
            ) -> crate::Result<()>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                #service_instance_env_stmt
                let node_name = messenger.node_name();
                #service_name_binding

                let mut service = peppylib::ServiceMessenger::listen(
                    messenger.handle(),
                    messenger.master_node(),
                    service_instance_id.as_str(),
                    node_name,
                    service_name,
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
        }
    };

    (method, helper_tokens)
}

fn build_request_struct_with_name_and_impl(
    struct_name: &str,
    params: &[FunctionParam],
    with_impl: bool,
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

    let tokens = if with_impl {
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

        quote! {
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
        }
    } else {
        quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            pub struct #ident {
                #( #field_tokens ),*
            }
        }
    };

    Some((ident, tokens))
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
    use_service_name_const: bool,
) -> TokenStream {
    let (context_expr, field_context_expr) = if use_service_name_const {
        (quote!(SERVICE_NAME), quote!(String::from(SERVICE_NAME)))
    } else {
        let context_literal = Literal::string(label);
        (
            quote!(#context_literal),
            quote!(String::from(#context_literal)),
        )
    };

    let reader_type = &request_spec.reader_type;

    // Common deserialization body builder
    let build_deserializer_body =
        |return_ty: TokenStream, result_expr: TokenStream, field_statements: Vec<TokenStream>| {
            quote! {
                fn #deserializer_fn_name(payload: &[u8]) -> crate::Result<#return_ty> {
                    let mut cursor = std::io::Cursor::new(payload);
                    let message_reader = capnp::serialize::read_message(
                            &mut cursor,
                            capnp::message::ReaderOptions::new(),
                        )
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#context_expr),
                            source,
                        })?;

                    let root = message_reader
                        .get_root::<#reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#context_expr),
                            source,
                        })?;

                    #(#field_statements)*

                    Ok(#result_expr)
                }
            }
        };

    if let Some(instance_param) = instance_id_param {
        let instance_ty = &instance_param.ty;
        let request_return_ty = build_return_type_from_params(handler_params, request_struct);
        let return_ty = quote!((#instance_ty, #request_return_ty));

        // Deserialize all wire params, separating instance_id from the rest
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
                &field_context_expr,
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

        let ordered_request_values: Vec<Ident> = handler_params
            .iter()
            .map(|param| {
                let key = param.ident.to_string();
                handler_value_map
                    .get(&key)
                    .unwrap_or_else(|| panic!("missing field `{key}` in request payload"))
                    .clone()
            })
            .collect();

        let request_expr =
            build_result_expr_from_values(handler_params, &ordered_request_values, request_struct);
        let result_expr = quote!((#instance_value_ident, #request_expr));

        build_deserializer_body(return_ty, result_expr, field_statements)
    } else {
        let return_ty = build_return_type_from_params(handler_params, request_struct);
        let (field_statements, value_idents) =
            deserialize_fields_from_format(request_format, wire_params, label, &field_context_expr);
        let request_expr =
            build_result_expr_from_values(handler_params, &value_idents, request_struct);

        build_deserializer_body(return_ty, request_expr, field_statements)
    }
}

fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    label: &str,
    callback_call: &TokenStream,
    service_instance_ident: Option<&Ident>,
    use_service_name_const: bool,
) -> TokenStream {
    let Some(spec) = response_spec else {
        return quote!({
            #callback_call?;
            bytes::Bytes::new()
        });
    };

    // For exposed services (use_service_name_const=true), use format! with SERVICE_NAME constant
    // For other handlers, use the label directly
    let error_context = if use_service_name_const {
        quote!(format!("handle_request_payload {}", SERVICE_NAME))
    } else {
        let context_literal = Literal::string(label);
        quote!(String::from(#context_literal))
    };
    let response_ident = Ident::new("response", Span::call_site());
    let serialization = build_response_payload_tokens(
        spec,
        &response_ident,
        &error_context,
        service_instance_ident,
    );

    quote!({
        let response = #callback_call?;
        #serialization
    })
}

// --- Action generation helpers ---

fn build_action_handle_struct(has_goal: bool, has_feedback: bool, has_result: bool) -> TokenStream {
    let mut fields = Vec::new();

    if has_goal {
        fields.push(quote!(goal_service: peppylib::messaging::ServiceEndpoint));
        fields.push(quote!(cancel_service: peppylib::messaging::ServiceEndpoint));
    }

    if has_result {
        fields.push(quote!(result_service: peppylib::messaging::ServiceEndpoint));
    }

    if has_feedback {
        fields.push(quote!(feedback_publisher: peppylib::messaging::TopicPublisher));
    }

    quote! {
        pub struct ActionHandle {
            #( #fields ),*
        }
    }
}

fn build_action_expose_method() -> TokenStream {
    quote! {
        pub async fn expose(messenger: &crate::Messenger) -> crate::Result<Self> {
            let action = peppylib::ActionMessenger::expose(
                messenger.handle(),
                messenger.runtime().bound_master_node(),
                messenger.runtime().bound_instance_id(),
                messenger.runtime().node_name(),
                ACTION_NAME,
            )
            .await?;

            Ok(Self {
                goal_service: action.goal_service,
                cancel_service: action.cancel_service,
                result_service: action.result_service,
                feedback_publisher: action.feedback_publisher,
            })
        }
    }
}

fn build_action_handle_method(
    method_name: &Ident,
    helper_name: &Ident,
    request_struct: &Ident,
    response_struct: &Ident,
    service_field: &Ident,
    has_payload: bool,
) -> TokenStream {
    let helper_call = if has_payload {
        quote! {
            let message = request_context.message();
            let payload = message.payload().as_bytes();
            let master_node = message.master_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(
                payload.as_ref(),
                &handler,
                master_node,
                instance_id,
            )
        }
    } else {
        quote! {
            let message = request_context.message();
            let master_node = message.master_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(&handler, master_node, instance_id)
        }
    };

    quote! {
        pub async fn #method_name<F>(
            &mut self,
            handler: F,
        ) -> crate::Result<bool>
        where
            F: Fn(#request_struct) -> crate::Result<#response_struct>,
        {
            let result = self
                .#service_field
                .handle_next_request(move |request_context| {
                    async move {
                        #helper_call
                        .map_err(|error| {
                            peppylib::PeppyError::Io(
                                std::io::Error::other(error.to_string()),
                            )
                        })
                    }
                })
                .await?;

            Ok(result)
        }
    }
}

fn build_action_payload_handler(
    handler_name: &Ident,
    deserializer_name: &Ident,
    request_struct: &Ident,
    request_data_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
    has_payload: bool,
) -> TokenStream {
    let response_ty = response_spec
        .as_ref()
        .map(|spec| {
            let struct_ident = &spec.struct_ident;
            quote!(#struct_ident)
        })
        .unwrap_or_else(|| quote!(()));

    let request_construction = if request_data_struct.is_some() {
        quote!(let request = #request_struct { instance_id, master_node, data: request_data };)
    } else {
        quote!(let request = #request_struct { instance_id, master_node };)
    };

    let response_serialization = if let Some(spec) = response_spec {
        let response_ident = Ident::new("response", Span::call_site());
        let error_context = quote!(format!("{} {}", stringify!(#handler_name), ACTION_NAME));
        let serialization =
            build_response_payload_tokens(spec, &response_ident, &error_context, None);

        quote!({
            let response = handler(request)?;
            #serialization
        })
    } else {
        quote!({
            handler(request)?;
            bytes::Bytes::new()
        })
    };

    if has_payload {
        quote! {
            fn #handler_name<F>(
                payload: &[u8],
                handler: &F,
                master_node: String,
                instance_id: String,
            ) -> crate::Result<bytes::Bytes>
            where
                F: Fn(#request_struct) -> crate::Result<#response_ty>,
            {
                let request_data = #deserializer_name(payload)?;
                #request_construction

                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        }
    } else {
        quote! {
            fn #handler_name<F>(
                handler: &F,
                master_node: String,
                instance_id: String,
            ) -> crate::Result<bytes::Bytes>
            where
                F: Fn(#request_struct) -> crate::Result<#response_ty>,
            {
                #request_construction

                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        }
    }
}

fn build_action_request_deserializer(
    deserializer_fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    request_format: &MessageFormat,
    params: &[FunctionParam],
    label: &str,
    request_struct: Option<&Ident>,
) -> TokenStream {
    let reader_type = &request_spec.reader_type;
    let return_ty = build_return_type_from_params(params, request_struct);
    let context_expr = quote!(format!(
        "{} {}",
        stringify!(#deserializer_fn_name),
        ACTION_NAME
    ));

    let (field_statements, value_idents) =
        deserialize_fields_from_format(request_format, params, label, &context_expr);

    let request_expr = build_result_expr_from_values(params, &value_idents, request_struct);

    quote! {
        fn #deserializer_fn_name(payload: &[u8]) -> crate::Result<#return_ty> {
            let mut cursor = std::io::Cursor::new(payload);
            let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: #context_expr,
                    source,
                })?;

            let root = message_reader
                .get_root::<#reader_type>()
                .map_err(|source| crate::Error::CapnpDeserialize {
                    context: #context_expr,
                    source,
                })?;

            #(#field_statements)*

            Ok(#request_expr)
        }
    }
}

fn build_action_feedback_emit(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
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
        quote!(&self)
    } else {
        quote!(&self, #(#method_param_tokens),*)
    };

    let label_literal = Literal::string(label);

    match encoding {
        Some(spec) => {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let root_ident = Ident::new("root", Span::call_site());

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn emit_feedback(#method_signature) -> crate::Result<()> {
                    let mut message = capnp::message::Builder::new_default();
                    {
                        let mut #root_ident = message.init_root::<#builder_type>();
                        #(#assignments)*
                    }

                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
                        crate::Error::CapnpSerialize {
                            context: format!("{} {}", #label_literal, ACTION_NAME),
                            source,
                        }
                    })?;

                    let payload = bytes::Bytes::from(buffer);
                    self.feedback_publisher.publish(payload).await?;
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
                pub async fn emit_feedback(#method_signature) -> crate::Result<()> {
                    #(#ignore_params)*
                    Err(crate::Error::MessageFormatUnavailable {
                        context: format!("{} {}", #label_literal, ACTION_NAME),
                    })
                }
            }
        }
    }
}

// --- End action generation helpers ---

// --- Shared deserialization/serialization helpers ---

/// Builds a return type token stream from function params.
fn build_return_type_from_params(
    params: &[FunctionParam],
    request_struct: Option<&Ident>,
) -> TokenStream {
    if let Some(request_struct) = request_struct {
        quote!(#request_struct)
    } else if params.is_empty() {
        quote!(())
    } else if params.len() == 1 {
        let ty = &params[0].ty;
        quote!((#ty,))
    } else {
        let types: Vec<&TokenStream> = params.iter().map(|p| &p.ty).collect();
        quote!((#(#types),*))
    }
}

/// Deserializes fields from a message format, returning statements and value identifiers.
fn deserialize_fields_from_format(
    request_format: &MessageFormat,
    params: &[FunctionParam],
    label: &str,
    context_expr: &TokenStream,
) -> (Vec<TokenStream>, Vec<Ident>) {
    let schema_lookup = SchemaFieldLookup::new(request_format);
    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut value_idents = Vec::new();

    for param in params {
        let field_key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&field_key);

        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            original_name.as_str(),
            schema,
            label,
            context_expr,
            &mut names,
        );
        field_statements.append(&mut statements);
        value_idents.push(value_ident);
    }

    (field_statements, value_idents)
}

/// Builds a result expression (struct or tuple) from deserialized value identifiers.
fn build_result_expr_from_values(
    params: &[FunctionParam],
    value_idents: &[Ident],
    request_struct: Option<&Ident>,
) -> TokenStream {
    if let Some(request_struct) = request_struct {
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
    }
}

/// Generates response serialization code from a response spec and format.
fn build_response_payload_tokens(
    spec: &ServiceResponseSpec,
    response_ident: &Ident,
    error_context: &TokenStream,
    service_instance_ident: Option<&Ident>,
) -> TokenStream {
    let builder_type = &spec.builder_type;
    let format = spec.format;
    let builder_ident = Ident::new("response_root", Span::call_site());

    let mut assignments = Vec::new();
    let mut names = NameGenerator::new();

    for (field_name, schema) in &format.0 {
        if spec.include_service_instance_id && field_name == "instance_id" {
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
        let mut message = capnp::message::Builder::new_default();
        {
            let mut #builder_ident = message.init_root::<#builder_type>();
            #( #assignments )*
        }
        let mut buffer = Vec::new();
        capnp::serialize::write_message(&mut buffer, &message).map_err(|source| {
            crate::Error::CapnpSerialize {
                context: #error_context,
                source,
            }
        })?;
        bytes::Bytes::from(buffer)
    })
}

// --- End shared helpers ---

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
