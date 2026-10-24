#[cfg(test)]
mod tests;

mod actions;
mod context;
mod deserialization;
mod failures;
mod fixtures;
pub(crate) mod identifiers;
mod mock;
mod parameters;
mod scaffold;
mod serialization;
mod services;
mod topics;
pub(crate) mod type_mapping;

pub(crate) use parameters::join_parameter_path;
pub use parameters::{generate_parameters_struct, validate_parameter_schema};

use super::types::{
    CapnpSchema, ConsumedActionMessage, ContractOrigin, DependencyContext, InterfaceArtifact,
    InterfaceKind, LanguageGenerator, PairTopicConsumerKind, member_failure_kinds,
    non_empty_message_format, scoped_schema_key,
};
use crate::error::{Error, Result};
use crate::generator::naming::{
    resolve_schema_file_stem, sanitize_component, sanitize_node_display_name, to_camel_case,
};
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService, TopicRetention,
};
use encoding::{CapnpSchemaArtifacts, FunctionParam};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;
use std::path::Path;

use actions::{
    ResultDelivery, build_action_expose_method, build_action_handle_struct,
    build_action_request_deserializer, build_goal_context_base_methods,
    build_goal_context_complete, build_goal_context_publish_feedback, build_goal_context_struct,
    build_goal_decision_enum, build_handle_goal_next_request,
};
use context::{
    GenerationContext, add_response_struct, build_fields_from_params, collect_function_params,
    map_declared_message_format, map_message_format,
};
use deserialization::{build_deserialize_fn, deserialize_format_fields};
use failures::{FailingMember, build_failure_items};
use serialization::{
    MessageEncodingSpec, build_serialize_payload, generate_assignments_for_format,
    generate_assignments_from_struct,
};
use services::{
    ExposedServiceMethodSpec, ServiceResponseSpec, build_exposed_service_method,
    build_request_struct, build_response_payload_tokens, build_result_expr_from_values,
    deserialize_fields_from_format,
};
use topics::{
    ConsumedTopicSubscriptionSpec, PeerTopicSubscriptionSpec, build_consumed_topic_subscription,
    build_observed_module_header, build_observed_topic_subscription, build_peer_module_header,
    build_peer_topic_publisher, build_peer_topic_subscription, build_topic_publisher,
    consumed_to_target_expression,
};
use type_mapping::render_tokens;

/// Rust-specific implementation of the interface generator.
#[derive(Default)]
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
    parameters: config::ParameterSchema,
    testgen: crate::generator::testgen::TestGenRegistry,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the node parameters for code generation.
    pub fn set_parameters(&mut self, parameters: config::ParameterSchema) {
        self.parameters = parameters;
    }

    /// Sets the node's manifest identity, enabling the generated `fixtures`
    /// surface (the harness pins the node's own targets with it).
    pub fn set_node_identity(&mut self, name: &str, tag: &str) {
        self.testgen.record_node_identity(name, tag);
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    #[cfg(test)]
    fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }

    fn register_schema(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: &CapnpSchemaArtifacts,
    ) -> Result<SchemaInfo> {
        let resolved = resolve_schema_file_stem(schema_key);
        let schema_source = artifacts.encoding_schema();

        // The same file stem is registered more than once only when two
        // callers describe the very same message: the consumer of a slot and
        // the mock publisher of that slot both resolve the slot's key. The
        // file is written once and every caller gets a Rust-side reference to
        // the struct that actually exists in it, so reuse the first
        // registration's struct identity. Any other collision means two
        // different formats want one file; that is a schema-key naming bug
        // and must fail here rather than hand a caller a schema its decode
        // code does not match.
        if let Some(existing) = self.schemas.get(&resolved.file_stem) {
            if existing.source() != schema_source {
                return Err(Error::SchemaFileStemCollision {
                    file_stem: resolved.file_stem,
                    first_key: existing.schema_key().to_string(),
                    second_key: schema_key.to_string(),
                });
            }
            return Ok(SchemaInfo {
                file_stem: resolved.file_stem,
                struct_module: existing.struct_module().to_string(),
            });
        }

        let struct_name = format!("{struct_prefix}Message");
        let struct_module = crate::generator::naming::normalize_snake_case(&struct_name);
        let schema = schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let capnp_schema = CapnpSchema::new(
            schema_key.to_string(),
            resolved.file_stem.clone(),
            struct_module.clone(),
            schema_source.to_string(),
            schema,
        );
        self.schemas
            .insert(resolved.file_stem.clone(), capnp_schema);

        Ok(SchemaInfo {
            file_stem: resolved.file_stem,
            struct_module,
        })
    }

    /// [`Self::prepare_declared_message_encoding`] for a format that may be
    /// absent: an absent format has no encoding.
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
        self.prepare_declared_message_encoding(schema_key, struct_prefix, artifacts, params)
            .map(Some)
    }

    /// Registers the schema of a declared message format and builds the
    /// assignments that serialize `params` into it.
    fn prepare_declared_message_encoding(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: &CapnpSchemaArtifacts,
        params: &[FunctionParam],
    ) -> Result<MessageEncodingSpec> {
        let schema_info = self.register_schema(schema_key, struct_prefix, artifacts)?;
        let builder_ident = Ident::new("root", Span::call_site());
        let assignments =
            generate_assignments_for_format(&builder_ident, artifacts.message_format(), params)?;

        Ok(MessageEncodingSpec {
            builder_type: schema_info.builder_type_tokens(),
            assignments,
            reader_type: schema_info.reader_type_tokens(),
        })
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
        // Each format is mapped under the exact key it is registered under so
        // the rendered schema (file id included) is identical to what the mock
        // and fixture generators produce for the same key.
        let request_artifacts = map_message_format(schema_key, request_format)?;
        let response_artifacts =
            map_message_format(&format!("{schema_key}_response"), response_format)?;

        // A goal with request data takes a `GoalRequest` argument and
        // serializes it; a goal without one sends an empty payload.
        let (request_param, goal_payload_tokens) = match &request_artifacts {
            Some(artifacts) => {
                let goal_request_ident = Ident::new("GoalRequest", Span::call_site());
                let params = collect_function_params(
                    Some(artifacts),
                    &format!("{action_struct_name}Goal"),
                    context,
                )?;
                context.add_struct(
                    goal_request_ident.clone(),
                    build_fields_from_params(&params),
                );

                let schema_info = self.register_schema(
                    schema_key,
                    &format!("{action_struct_name}GoalMessage"),
                    artifacts,
                )?;
                let builder_type = schema_info.builder_type_tokens();

                let request_ident = Ident::new("request", Span::call_site());
                let assignments = generate_assignments_from_struct(
                    &Ident::new("root", Span::call_site()),
                    artifacts.message_format(),
                    &request_ident,
                )?;

                let error_context = quote!(format!(
                    "fire_goal {} {}",
                    TARGET_NODE_NAME, TARGET_ACTION_NAME
                ));
                let serialize_block =
                    build_serialize_payload(&builder_type, &[], &assignments, &error_context);
                (
                    quote!(request: #goal_request_ident,),
                    quote! { let goal_payload = #serialize_block; },
                )
            }
            None => (
                quote!(),
                quote! {
                    let goal_payload = peppylib::Payload::new();
                },
            ),
        };

        let goal_response_data_ident = Ident::new("GoalResponseData", Span::call_site());
        let has_goal_response_data = response_artifacts.is_some();

        // The response-specific parts: the GoalResponseData struct, its
        // deserializer, and the `data` binding and field of the returned
        // handle.
        let mut helper_items = Vec::new();
        let (data_binding, data_field) = match &response_artifacts {
            Some(response_artifacts) => {
                add_response_struct(context, response_artifacts, &goal_response_data_ident)?;

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
                let (response_statements, response_inits) =
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

                // An accepted goal carries the declared response; a refused
                // one carries none.
                let data_binding = quote! {
                    let data = match &reply.answer {
                        peppylib::messaging::GoalAnswer::Accepted(response) => {
                            Some(deserialize_goal_response(response.as_ref())?)
                        }
                        peppylib::messaging::GoalAnswer::Refused(_) => None,
                    };
                };
                (data_binding, quote!(data,))
            }
            None => (TokenStream::new(), TokenStream::new()),
        };
        // The refusal is parsed into the member's `Failure`: a kind the
        // member does not declare is an error, not a refusal.
        let response_handling = quote! {
            let reply = action_handle.goal_reply();
            let refusal = reply.refusal().map(Failure::from_wire).transpose()?;
            #data_binding
            Ok(Self {
                messenger: node_runner.messenger().clone(),
                accepted: refusal.is_none(),
                inner: action_handle,
                refusal,
                #data_field
            })
        };

        // The `to_target` matches the producer's emission shape: a dependency
        // with a `ContractOrigin` is addressed as
        // `SenderTarget::contract(...)`, otherwise as its native node
        // identity. The `target` is the caller-selected member of the slot's
        // bound producer set (its full `(core_node, instance_id)`), checked
        // against the set before anything reaches the wire and then addressed
        // directly with no discovery. The returned handle retains it: goal
        // submission, feedback, result retrieval, and cancellation all stay
        // pinned to that same producer.
        let to_target_expr = consumed_to_target_expression(dependency);
        let target_selection_doc = doc_attrs(dependency.target_selection_doc());
        let method_tokens = quote! {
            /// Fires this goal at `target`, a member of this slot's bound set (a
            /// `ProducerRef` yielded by the slot's own subscription is a member
            /// by construction); a target outside the set fails before anything
            /// reaches the wire. Each returned handle stays pinned to its
            /// target: feedback, result retrieval, and cancellation all address
            /// that producer directly.
            ///
            /// The handle keeps every feedback message until it is read, up to
            /// `feedback_bound` unread messages (`DEFAULT_FEEDBACK_BOUND` holds
            /// 1024). A caller that falls behind by more cancels the goal, and
            /// reads `Error::ActionFeedbackOverflow`.
            ///
            #(#target_selection_doc)*
            pub async fn fire_goal(
                node_runner: &crate::NodeRunner,
                target: &peppylib::messaging::ProducerRef,
                timeout: std::time::Duration,
                #request_param
                feedback_bound: peppylib::messaging::FeedbackBound,
            ) -> crate::Result<Self> {
                node_runner.processor().ensure_target_bound(LINK_ID, target)?;

                #goal_payload_tokens

                let action_handle = peppylib::ActionMessenger::send_goal(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    #to_target_expr,
                    TARGET_ACTION_NAME,
                    Some(target),
                    goal_payload,
                    feedback_bound,
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
    /// `get_result` returns a typed `ResultResponse` carrying a `ResultOutcome`
    /// enum (Completed/Cancelled with the decoded result data, Failed with the
    /// member's `Failure`, or the empty Abandoned/Expired terminal states).
    /// `ActionMessenger::request_result` decodes the engine's framing into a
    /// `peppylib::messaging::GoalOutcome`, so this only decodes the result
    /// bodies and parses the failure.
    fn build_consumed_action_get_result_method(
        &mut self,
        context: &mut GenerationContext,
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

        let decoder = match result_artifacts {
            Some(result_artifacts) => {
                add_response_struct(context, &result_artifacts, &data_ident)?;

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
                let (statements, inits) = deserialize_format_fields(
                    response_format,
                    "ResultResponseData",
                    &context_expr,
                )?;
                let deserialize_fn_ident =
                    Ident::new("deserialize_result_response", Span::call_site());
                helper_items.push(build_deserialize_fn(
                    &deserialize_fn_ident,
                    &reader_type,
                    &context_expr,
                    &quote!(#data_ident),
                    &statements,
                    &quote!(#data_ident { #( #inits ),* }),
                ));
                Some(ResultDecoder {
                    data_ident,
                    deserialize_fn_ident,
                })
            }
            None => None,
        };

        let ResultOutcomeTokens { variants, decode } =
            build_result_outcome(&outcome_ident, decoder.as_ref());
        helper_items.push(quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            pub enum #outcome_ident {
                #variants
            }
        });
        helper_items.push(response_struct);

        let method_tokens = quote! {
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
                #decode
                Ok(#response_ident {
                    instance_id: reply.instance_id,
                    core_node: reply.core_node,
                    outcome,
                })
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
                let state = match peppylib::messaging::decode_cancel_ack(response.payload_bytes().as_ref())? {
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
        let format_artifacts = map_declared_message_format(schema_key, format)?;

        let struct_prefix = format!("{action_struct_name}Feedback");
        let schema_struct_name = format!("{struct_prefix}Message");
        let struct_ident = Ident::new("FeedbackMessage", Span::call_site());

        let params =
            collect_function_params(Some(&format_artifacts), &schema_struct_name, context)?;

        context.add_struct(struct_ident.clone(), build_fields_from_params(&params));

        let encoding = self.prepare_declared_message_encoding(
            schema_key,
            &struct_prefix,
            &format_artifacts,
            &params,
        )?;
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
        let helper_tokens = build_deserialize_fn(
            &helper_fn_ident,
            &reader_type,
            &feedback_context_expr,
            &quote!(#struct_ident),
            &field_statements,
            &build_result_expr_from_values(&params, &value_idents, Some(&struct_ident)),
        );

        let method_ident = Ident::new("on_next_feedback_message", Span::call_site());
        let method_tokens = quote! {
            /// Receives the next feedback message for this goal. The handle
            /// keeps every message until it is read, up to the feedback bound
            /// of `fire_goal`.
            ///
            /// Terminal errors that end a feedback drain loop:
            /// `Error::ActionFeedbackChannelClosed` when the producer closed
            /// the stream cleanly (end-of-stream sentinel),
            /// `Error::ActionFeedbackProducerGone` when the producer instance
            /// disappeared without closing it (process killed, crashed), in
            /// which case `get_result` resolves to `ResultOutcome::Abandoned`,
            /// and `Error::ActionFeedbackOverflow` when this caller fell behind
            /// by more unread messages than its bound, in which case the
            /// handle cancelled the goal.
            pub async fn #method_ident(
                &mut self,
            ) -> crate::Result<#struct_ident> {
                let feedback = self.inner.on_next_feedback().await?;
                let payload = feedback.payload_bytes();
                #helper_fn_ident(payload.as_ref())
            }
        };

        Ok((method_tokens, vec![helper_tokens]))
    }
}

/// The struct an action's result reply decodes into and the generated
/// function that decodes it from the reply body.
struct ResultDecoder {
    data_ident: Ident,
    deserialize_fn_ident: Ident,
}

/// The variants of a `ResultOutcome` enum and the `let outcome = match ...;`
/// statement that maps a result reply's `GoalOutcome` onto them.
struct ResultOutcomeTokens {
    variants: TokenStream,
    decode: TokenStream,
}

/// Builds the `ResultOutcome` mapping shared by the production `get_result`
/// and the fixture caller of an exposed action. With a `decoder`, the
/// completed and cancelled outcomes carry the decoded result data; without
/// one, they carry none. A cancelled outcome always carries its reason, and
/// a failed one the member's `Failure` (parsed from the wire, so a kind the
/// member does not declare is an error). Abandoned and expired outcomes
/// carry nothing.
fn build_result_outcome(
    outcome_ident: &Ident,
    decoder: Option<&ResultDecoder>,
) -> ResultOutcomeTokens {
    let (completed_variant, cancelled_variant, completed_arm, cancelled_arm) = match decoder {
        Some(ResultDecoder {
            data_ident,
            deserialize_fn_ident,
        }) => (
            quote!(Completed(#data_ident)),
            quote!(Cancelled { reason: String, data: #data_ident }),
            quote! {
                peppylib::messaging::GoalOutcome::Completed(result) => {
                    #outcome_ident::Completed(#deserialize_fn_ident(result.as_ref())?)
                }
            },
            quote! {
                peppylib::messaging::GoalOutcome::Cancelled { reason, result } => {
                    #outcome_ident::Cancelled {
                        reason,
                        data: #deserialize_fn_ident(result.as_ref())?,
                    }
                }
            },
        ),
        None => (
            quote!(Completed),
            quote!(Cancelled { reason: String }),
            quote!(peppylib::messaging::GoalOutcome::Completed(_) => #outcome_ident::Completed,),
            quote! {
                peppylib::messaging::GoalOutcome::Cancelled { reason, .. } => {
                    #outcome_ident::Cancelled { reason }
                }
            },
        ),
    };
    ResultOutcomeTokens {
        variants: quote! {
            /// The goal is a success.
            #completed_variant,
            /// The goal ended cancelled, for `reason`.
            #cancelled_variant,
            /// The goal failed: a failure kind that the action declares,
            /// a message, and no result.
            Failed(Failure),
            /// The goal ended without a result: its worker dropped it, or
            /// the provider is gone.
            Abandoned,
            /// The goal ended, and the provider no longer keeps its result.
            Expired,
        },
        decode: quote! {
            let outcome = match reply.outcome {
                #completed_arm
                #cancelled_arm
                peppylib::messaging::GoalOutcome::Failed(failure) => {
                    #outcome_ident::Failed(Failure::from_wire(&failure)?)
                }
                peppylib::messaging::GoalOutcome::Abandoned => #outcome_ident::Abandoned,
                peppylib::messaging::GoalOutcome::Expired => #outcome_ident::Expired,
            };
        },
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

/// The name of the struct a topic subscription decodes into, also the prefix
/// of the structs nested in it.
const SUBSCRIBED_MESSAGE_STRUCT_NAME: &str = "Message";

/// The `Message` struct a topic subscription decodes into: the context that
/// holds it and its nested structs, its fields as parameters, the name of
/// the helper that decodes it and the encoding registered for its schema.
struct SubscribedMessage {
    context: GenerationContext,
    params: Vec<FunctionParam>,
    struct_ident: Ident,
    helper_fn_ident: Ident,
    encoding: MessageEncodingSpec,
}

impl RustGenerator {
    /// Prepares the [`SubscribedMessage`] of a topic schema. The bound-set
    /// and the pairing consumers both build their subscription on it, so a
    /// topic decodes into the same struct through the same helper whichever
    /// way it is consumed.
    fn prepare_subscribed_message(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        format_artifacts: &CapnpSchemaArtifacts,
    ) -> Result<SubscribedMessage> {
        let mut context = GenerationContext::default();
        let params = collect_function_params(
            Some(format_artifacts),
            SUBSCRIBED_MESSAGE_STRUCT_NAME,
            &mut context,
        )?;

        let struct_ident = Ident::new(SUBSCRIBED_MESSAGE_STRUCT_NAME, Span::call_site());
        context.add_struct(struct_ident.clone(), build_fields_from_params(&params));

        let encoding = self.prepare_declared_message_encoding(
            schema_key,
            struct_prefix,
            format_artifacts,
            &params,
        )?;
        Ok(SubscribedMessage {
            context,
            params,
            struct_ident,
            helper_fn_ident: Ident::new("deseralize_payload", Span::call_site()),
            encoding,
        })
    }

    /// Shared consume-side scaffold for peer and observer pairing topics. The
    /// two modes differ only in their module header, subscription runtime, and
    /// artifact classification.
    fn add_pair_topic_consumer(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
        kind: PairTopicConsumerKind,
    ) -> Result<()> {
        let schema_key =
            crate::generator::naming::peer_schema_key(&peer.link_id, topic.name.as_str());
        let struct_prefix = to_camel_case(&schema_key);

        let format_artifacts = map_message_format(&schema_key, topic.message_format.as_ref())?
            .ok_or_else(|| Error::PeerTopicMissingMessageFormat {
                link_id: peer.link_id.clone(),
                topic: topic.name.clone(),
            })?;

        let message =
            self.prepare_subscribed_message(&schema_key, &struct_prefix, &format_artifacts)?;
        let spec = PeerTopicSubscriptionSpec {
            helper_fn_ident: &message.helper_fn_ident,
            args_struct_ident: &message.struct_ident,
            params: &message.params,
            artifacts: &format_artifacts,
            encoding: &message.encoding,
            qos_profile: &topic.qos_profile,
            struct_prefix: SUBSCRIBED_MESSAGE_STRUCT_NAME,
        };
        let (header_tokens, method_tokens, artifact_kind) = match kind {
            PairTopicConsumerKind::Peer => (
                build_peer_module_header(topic.name.as_str(), peer),
                build_peer_topic_subscription(spec)?,
                InterfaceKind::PeerConsumedTopic,
            ),
            PairTopicConsumerKind::Observed(cardinality) => (
                build_observed_module_header(topic.name.as_str(), peer, cardinality),
                build_observed_topic_subscription(spec, cardinality)?,
                InterfaceKind::ObservedTopic,
            ),
        };
        let mut items = message.context.into_tokens();
        items.push(method_tokens);
        let rendered = render_tokens(quote! {
            #header_tokens
            #( #items )*
        });
        self.push_pair_topic_section(topic, peer, artifact_kind, rendered)
    }

    /// Pushes the module of a pairing topic at `[link_id, leaf]`, where the leaf
    /// is the topic name sanitized into a Rust module name, after checking that
    /// no other pairing artifact of the node already sits there.
    fn push_pair_topic_section(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
        kind: InterfaceKind,
        code: String,
    ) -> Result<()> {
        let topic_component = sanitize_component(topic.name.as_str());
        let module_path = peer.module_path_for(&sanitize_node_display_name(&topic_component));
        crate::generator::types::ensure_no_peer_collision(
            &self.sections,
            &module_path,
            peer,
            topic,
        )?;
        self.push_section(InterfaceArtifact {
            module_path,
            kind,
            code_output: code,
        });
        Ok(())
    }
}

impl LanguageGenerator for RustGenerator {
    fn add_emitted_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        origin: Option<&ContractOrigin>,
    ) -> Result<()> {
        self.testgen.record_emitted_topic(topic, origin);
        let fn_name_str = identifiers::rust_identifier_or(topic.name.as_str(), "topic");

        let schema_prefix = to_camel_case(&fn_name_str);
        let struct_prefix = String::from("Message");

        let mut context = GenerationContext::default();
        let scoped_key = scoped_schema_key(origin, &fn_name_str);
        let format_artifacts = map_message_format(&scoped_key, topic.message_format.as_ref())?;
        let params =
            collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
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

        self.push_section(InterfaceArtifact::for_leaf(
            origin,
            &sanitize_node_display_name(&module_label),
            InterfaceKind::EmittedTopic,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_service(
        &mut self,
        service: &NativeExposedService,
        origin: Option<&ContractOrigin>,
    ) -> Result<()> {
        self.testgen.record_exposed_service(service, origin);
        let fn_name_str = identifiers::rust_identifier_or(service.name.as_str(), "service");
        let struct_prefix = to_camel_case(&fn_name_str);
        let generic_response_ident = Ident::new("Response", Span::call_site());

        let mut context = GenerationContext::default();
        let request_format = non_empty_message_format(service.request_message_format.as_ref());
        let scoped_request_key = scoped_schema_key(origin, &fn_name_str);
        let scoped_response_key = scoped_schema_key(origin, &format!("{fn_name_str}_response"));
        let request_wire_artifacts = map_message_format(&scoped_request_key, request_format)?;
        let response_artifacts = map_message_format(
            &scoped_response_key,
            service.response_message_format.as_ref(),
        )?;
        if let Some(response_artifacts) = response_artifacts.as_ref() {
            add_response_struct(&mut context, response_artifacts, &generic_response_ident)?;
        }
        let wire_params = collect_function_params(
            request_wire_artifacts.as_ref(),
            &struct_prefix,
            &mut context,
        )?;
        let encoding = self.prepare_message_encoding(
            &scoped_request_key,
            &struct_prefix,
            request_wire_artifacts.as_ref(),
            &wire_params,
        )?;

        let request_data_struct_ident =
            if let Some((ident, tokens)) = build_request_struct("RequestData", &wire_params) {
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

        let response_spec = if let Some(return_artifacts) = response_artifacts.as_ref() {
            let response_prefix = format!("{struct_prefix}Response");
            let schema_info =
                self.register_schema(&scoped_response_key, &response_prefix, return_artifacts)?;
            Some(ServiceResponseSpec {
                format: return_artifacts.message_format(),
                struct_ident: generic_response_ident.clone(),
                builder_type: schema_info.builder_type_tokens(),
            })
        } else {
            None
        };

        let (method_token, helper_tokens) =
            build_exposed_service_method(&ExposedServiceMethodSpec {
                params: &wire_params,
                encoding: encoding.as_ref(),
                request_format,
                struct_prefix: &struct_prefix,
                request_struct: &request_struct_ident,
                request_data_struct: request_data_struct_ident.as_ref(),
                response_spec: response_spec.as_ref(),
                origin,
            })?;

        let service_name_const = {
            let service_name_str = Literal::string(service.name.as_str());
            quote!(const SERVICE_NAME: &str = #service_name_str;)
        };

        let mut service_tokens = vec![
            service_name_const,
            build_failure_items(
                &member_failure_kinds(origin),
                &quote!(SERVICE_NAME),
                FailingMember::Service,
            ),
        ];
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
        self.push_section(InterfaceArtifact::for_leaf(
            origin,
            &sanitize_node_display_name(&module_label),
            InterfaceKind::ExposedService,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_action(
        &mut self,
        action: &NativeExposedAction,
        origin: Option<&ContractOrigin>,
    ) -> Result<()> {
        self.testgen.record_exposed_action(action, origin);
        let base_name = identifiers::rust_identifier_or(&action.name, "action");
        let action_prefix = to_camel_case(&base_name);

        let mut context = GenerationContext::default();
        // Methods on `impl GoalContext` (request/goal_id/cancel_signal/fail
        // plus publish_feedback when applicable, and complete /
        // complete_cancelled).
        let mut goal_context_methods: Vec<TokenStream> = Vec::new();
        let mut helper_tokens: Vec<TokenStream> = Vec::new();
        // Free items (the failure kinds and the GoalDecision enum).
        let mut extra_items: Vec<TokenStream> = vec![build_failure_items(
            &member_failure_kinds(origin),
            &quote!(ACTION_NAME),
            FailingMember::Action,
        )];

        let action_name_literal = Literal::string(&action.name);

        let has_feedback = action.feedback_topic.is_some();

        // Every action is goal-driven in the concurrent model: clients fire a
        // goal, the server accepts/rejects and (when accepted) drives it through
        // a GoalContext. An absent `goal_service` simply means the goal carries
        // no request/response payload. The block yields the
        // `handle_goal_next_request` method of `impl ActionHandle`.
        let handle_goal_method = {
            let goal_service = action.goal_service.as_ref();
            let label = format!("{base_name}_goal");
            let schema_struct_prefix = format!("{action_prefix}Goal");
            let scoped_goal_key = scoped_schema_key(origin, &label);
            let scoped_goal_response_key = scoped_schema_key(origin, &format!("{label}_response"));

            let request_artifacts = map_message_format(
                &scoped_goal_key,
                goal_service.and_then(|goal| goal.request_message_format.as_ref()),
            )?;
            let response_artifacts = map_message_format(
                &scoped_goal_response_key,
                goal_service.and_then(|goal| goal.response_message_format.as_ref()),
            )?;

            // Generates `GoalResponse` (+ `new`) when there is a response, and
            // collects the request params used to build `GoalRequestData`. The
            // request's nested message structs are defined under this prefix.
            let goal_struct_prefix = "Goal";
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                add_response_struct(
                    &mut context,
                    response_artifacts,
                    &Ident::new("GoalResponse", Span::call_site()),
                )?;
            }
            let goal_data_params = collect_function_params(
                request_artifacts.as_ref(),
                goal_struct_prefix,
                &mut context,
            )?;

            let goal_request_data_struct = if let Some((ident, tokens)) =
                build_request_struct("GoalRequestData", &goal_data_params)
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
                    goal_struct_prefix,
                    goal_request_data_struct.as_ref(),
                )?;
                helper_tokens.push(deserializer_fn);
            }

            // The user closure returns this to accept or reject the goal.
            extra_items.push(build_goal_decision_enum(response_artifacts.is_some()));

            // Serialization for the accepted/rejected `GoalResponse` (reads a
            // local `response` variable). `None` when the goal has no response.
            let response_serialization = if let Some(return_artifacts) = response_artifacts.as_ref()
            {
                let response_schema_prefix = format!("{schema_struct_prefix}Response");
                let schema_info = self.register_schema(
                    &scoped_goal_response_key,
                    &response_schema_prefix,
                    return_artifacts,
                )?;
                let spec = ServiceResponseSpec {
                    format: return_artifacts.message_format(),
                    struct_ident: Ident::new("GoalResponse", Span::call_site()),
                    builder_type: schema_info.builder_type_tokens(),
                };
                let response_ident = Ident::new("response", Span::call_site());
                let error_context =
                    quote!(format!("{} {}", "handle_goal_next_request", ACTION_NAME));
                Some(build_response_payload_tokens(
                    &spec,
                    &response_ident,
                    &error_context,
                )?)
            } else {
                None
            };

            build_handle_goal_next_request(
                goal_request_data_struct.is_some(),
                response_serialization,
            )
        };

        // Every action hands out a GoalContext with these base methods.
        goal_context_methods.push(build_goal_context_base_methods());

        // Feedback → `GoalContext::publish_feedback`.
        if let Some(feedback) = action.feedback_topic.as_ref() {
            let label = format!("publish_feedback {}", action.name);
            let struct_prefix = format!("{action_prefix}Feedback");
            let feedback_schema_key =
                scoped_schema_key(origin, &format!("emit_{base_name}_feedback"));
            let format_artifacts =
                map_declared_message_format(&feedback_schema_key, &feedback.message_format)?;
            let params =
                collect_function_params(Some(&format_artifacts), &struct_prefix, &mut context)?;
            let encoding = self.prepare_declared_message_encoding(
                &feedback_schema_key,
                &struct_prefix,
                &format_artifacts,
                &params,
            )?;

            goal_context_methods.push(build_goal_context_publish_feedback(
                &params, &encoding, &label,
            ));
        }

        // Result → `GoalContext::complete` / `complete_cancelled`. An action
        // that declares no result ends its goals with an empty one.
        let result_format = action
            .result_service
            .as_ref()
            .and_then(|result| result.response_message_format.as_ref());
        let label = format!("{base_name}_result");
        let schema_struct_prefix = format!("{action_prefix}Result");
        let scoped_result_key = scoped_schema_key(origin, &format!("{label}_response"));
        let response_artifacts = map_message_format(&scoped_result_key, result_format)?;

        // The result-response fields become `complete(fields…)` parameters.
        let result_params = collect_function_params(
            response_artifacts.as_ref(),
            &schema_struct_prefix,
            &mut context,
        )?;
        let encoding = self.prepare_message_encoding(
            &scoped_result_key,
            &schema_struct_prefix,
            response_artifacts.as_ref(),
            &result_params,
        )?;
        for delivery in [ResultDelivery::Completed, ResultDelivery::Cancelled] {
            goal_context_methods.push(build_goal_context_complete(
                delivery,
                &result_params,
                encoding.as_ref(),
                &label,
            ));
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
                #handle_goal_method
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
        self.push_section(InterfaceArtifact::for_leaf(
            origin,
            &sanitize_node_display_name(&action.name),
            InterfaceKind::ExposedAction,
            rendered,
        ));
        Ok(())
    }

    fn add_consumed_topic(
        &mut self,
        topic: &ConsumedTopic,
        arguments: MessageFormat,
        retention: TopicRetention,
        dependency: &DependencyContext,
    ) -> Result<()> {
        self.testgen
            .record_consumed_topic(topic, &arguments, retention, dependency);
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

        let schema_key =
            crate::generator::naming::consumed_topic_schema_key(node_name, topic.name.as_str());

        let format_artifacts = map_declared_message_format(&schema_key, &arguments)?;

        let message =
            self.prepare_subscribed_message(&schema_key, &struct_prefix, &format_artifacts)?;
        let method_tokens = build_consumed_topic_subscription(ConsumedTopicSubscriptionSpec {
            helper_fn_ident: &message.helper_fn_ident,
            args_struct_ident: &message.struct_ident,
            params: &message.params,
            artifacts: &format_artifacts,
            encoding: &message.encoding,
            topic,
            retention,
            struct_prefix: SUBSCRIBED_MESSAGE_STRUCT_NAME,
            dependency,
        })?;
        let mut items = message.context.into_tokens();
        items.push(method_tokens);

        let tokens: TokenStream = quote! {
            #( #items )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(InterfaceArtifact {
            module_path: vec![topic.link_id.clone(), topic.name.clone()],
            kind: InterfaceKind::ConsumedTopic,
            code_output: rendered,
        });

        Ok(())
    }

    fn add_consumed_service(
        &mut self,
        service: &ConsumedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()> {
        self.testgen.record_consumed_service(
            service,
            request_arguments,
            response_arguments,
            dependency,
        );
        let dependency_node_name = dependency.producer_name.as_str();
        let request_arguments = non_empty_message_format(Some(request_arguments));
        let response_arguments = non_empty_message_format(Some(response_arguments));

        let method_label = crate::generator::naming::consumed_service_request_schema_key(
            dependency_node_name,
            service.name.as_str(),
        );
        let response_schema_key = crate::generator::naming::consumed_service_response_schema_key(
            dependency_node_name,
            service.name.as_str(),
        );

        // Mapped under the exact keys they are registered under so the
        // rendered schemas (file ids included) match the mock's for this slot.
        let request_artifacts = map_message_format(&method_label, request_arguments)?;
        let response_artifacts = map_message_format(&response_schema_key, response_arguments)?;
        let struct_prefix = identifiers::consumed_service_struct_prefix(service.name.as_str());
        let method_ident = Ident::new("poll", Span::call_site());

        let mut context = GenerationContext::default();
        let response_data_ident = Ident::new("ResponseData", Span::call_site());

        if let Some(response_artifacts) = response_artifacts.as_ref() {
            add_response_struct(&mut context, response_artifacts, &response_data_ident)?;
        }
        let params =
            collect_function_params(request_artifacts.as_ref(), &struct_prefix, &mut context)?;

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
            context.add_struct_with_constructor(
                request_struct_ident.clone(),
                build_fields_from_params(&request_struct_params),
            );
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
            quote! {
                let request_payload = peppylib::Payload::new();
            }
        };

        // The `to_target` matches the producer's emission shape: if the
        // dependency exposes the service via an implemented contract, address it as the
        // interface; otherwise as the dependency's node identity. The
        // `target` is the caller-selected member of the slot's bound
        // producer set (its full `(core_node, instance_id)`), checked
        // against the set before anything reaches the wire and then
        // addressed directly with no discovery.
        let to_target_expr = consumed_to_target_expression(dependency);
        // An error reply reaches the caller as `Error::ServiceFailed`, which
        // `Failure::of` reads; one with a kind this member does not declare
        // is a deserialization error.
        let poll_call = quote! {
            peppylib::ServiceMessenger::poll(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #to_target_expr,
                SERVICE_NAME,
                peppylib::messaging::ServiceTarget::Producer(target),
                request_payload,
                timeout,
            )
            .await
            .map_err(Failure::reject_undeclared_kind)
        };

        let response_struct_ident = Ident::new("Response", Span::call_site());

        let (return_ty, response_tokens, poll_tokens, deserialize_fn_tokens) =
            if let Some(response_artifacts) = response_artifacts.as_ref() {
                let response_struct_name = format!("{struct_prefix}Response");

                let response_schema = self.register_schema(
                    &response_schema_key,
                    &response_struct_name,
                    response_artifacts,
                )?;
                let response_reader_type = response_schema.reader_type_tokens();

                let response_format = response_artifacts.message_format();
                let field_context_expr = quote!(context.clone());
                let (response_statements, response_inits) = deserialize_format_fields(
                    response_format,
                    &response_data_ident.to_string(),
                    &field_context_expr,
                )?;

                let poll_tokens = quote! {
                    let response_message = #poll_call?;
                };

                context
                    .add_metadata_struct(response_struct_ident.clone(), Some(&response_data_ident));

                let response_tokens = quote! {
                    let payload = response_message.payload_bytes();
                    let response_data = deserialize_response(payload.as_ref())?;
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
                    let _ = #poll_call?;
                };
                (quote!(()), quote!(Ok(())), poll_tokens, None)
            };

        let mut service_tokens = context.into_tokens();
        let service_name_literal = Literal::string(service.name.as_str());
        let node_name_literal = Literal::string(dependency_node_name);
        let link_id_literal = Literal::string(&dependency.link_id);

        let constants_tokens = quote! {
            const NODE_NAME: &str = #node_name_literal;
            const SERVICE_NAME: &str = #service_name_literal;
            const LINK_ID: &str = #link_id_literal;
        };

        let mut fn_param_tokens = vec![
            quote!(node_runner: &crate::NodeRunner),
            quote!(target: &peppylib::messaging::ProducerRef),
            quote!(timeout: std::time::Duration),
        ];
        if !request_struct_params.is_empty() {
            fn_param_tokens.push(quote!(request: #request_struct_ident));
        }

        let bound_producer_accessor_fn =
            crate::generator::rust::topics::build_bound_producer_accessor_fn(dependency);

        let target_selection_doc = doc_attrs(dependency.target_selection_doc());
        let function_token = quote! {
            /// Polls this service on `target`, a member of this slot's bound set
            /// (a `ProducerRef` yielded by the slot's own subscription is a
            /// member by construction); a target outside the set fails before
            /// anything reaches the wire.
            ///
            /// An error reply of the provider is `Error::ServiceFailed`:
            /// `Failure::of` reads its failure, with a kind that this member
            /// declares.
            ///
            #(#target_selection_doc)*
            pub async fn #method_ident(#(#fn_param_tokens),*) -> crate::Result<#return_ty> {
                node_runner.processor().ensure_target_bound(LINK_ID, target)?;

                #request_payload_tokens

                #poll_tokens

                #response_tokens
            }
        };

        let mut all_tokens = vec![
            constants_tokens,
            bound_producer_accessor_fn,
            build_failure_items(
                &dependency.member_failure_kinds(),
                &quote!(SERVICE_NAME),
                FailingMember::Service,
            ),
        ];
        all_tokens.append(&mut service_tokens);
        all_tokens.push(function_token);
        if let Some(deserialize_fn) = deserialize_fn_tokens {
            all_tokens.push(deserialize_fn);
        }

        let tokens: TokenStream = quote! {
            #( #all_tokens )*
        };
        let rendered = render_tokens(tokens);

        self.push_section(InterfaceArtifact {
            module_path: vec![service.link_id.clone(), service.name.clone()],
            kind: InterfaceKind::ConsumedService,
            code_output: rendered,
        });
        Ok(())
    }

    fn add_peer_emitted_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
    ) -> Result<()> {
        self.testgen.record_peer_emitted_topic(topic, peer);
        let schema_key =
            crate::generator::naming::peer_schema_key(&peer.link_id, topic.name.as_str());
        let schema_prefix = to_camel_case(&schema_key);
        let struct_prefix = String::from("Message");

        let mut context = GenerationContext::default();
        let format_artifacts = map_message_format(&schema_key, topic.message_format.as_ref())?;
        let params =
            collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
        let encoding = self.prepare_message_encoding(
            &schema_key,
            &schema_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();
        let header_tokens = build_peer_module_header(topic.name.as_str(), peer);
        let method_tokens = build_peer_topic_publisher(
            &params,
            encoding.as_ref(),
            &topic.qos_profile,
            &schema_key,
            peer.cardinality,
        );

        let tokens: TokenStream = quote! {
            #header_tokens
            #( #struct_tokens )*
            #method_tokens
        };
        let rendered = render_tokens(tokens);
        self.push_pair_topic_section(topic, peer, InterfaceKind::PeerEmittedTopic, rendered)
    }

    fn add_peer_consumed_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
    ) -> Result<()> {
        self.testgen.record_peer_consumed_topic(topic, peer);
        self.add_pair_topic_consumer(topic, peer, PairTopicConsumerKind::Peer)
    }

    fn add_observed_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        observer: &crate::generator::types::PeerContext,
        cardinality: Cardinality,
    ) -> Result<()> {
        self.testgen
            .record_observed_topic(topic, observer, cardinality);
        self.add_pair_topic_consumer(
            topic,
            observer,
            PairTopicConsumerKind::Observed(cardinality),
        )
    }

    fn add_consumed_action(
        &mut self,
        action: &ConsumedAction,
        messages: &ConsumedActionMessage,
        dependency: &DependencyContext,
    ) -> Result<()> {
        self.testgen
            .record_consumed_action(action, messages, dependency);
        let dependency_node_name = dependency.producer_name.as_str();
        let action_prefix =
            identifiers::consumed_action_type_prefix(dependency_node_name, action.name.as_str());
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
        let link_id_literal = Literal::string(&dependency.link_id);

        let goal_request_format = non_empty_message_format(messages.goal_request.as_ref());
        let goal_response_format = non_empty_message_format(messages.goal_response.as_ref());
        let feedback_format = non_empty_message_format(messages.feedback.as_ref());
        let result_response_format = non_empty_message_format(messages.result_response.as_ref());
        let target_node_name_constant = if goal_request_format.is_some()
            || goal_response_format.is_some()
            || feedback_format.is_some()
            || result_response_format.is_some()
        {
            quote!(const TARGET_NODE_NAME: &str = #node_name_literal;)
        } else {
            TokenStream::new()
        };
        let constants_tokens = quote! {
            #target_node_name_constant
            const TARGET_ACTION_NAME: &str = #action_name_literal;
            const LINK_ID: &str = #link_id_literal;
        };

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
            result_artifacts,
            &action_schema_keys.result_response,
        )?;
        methods.push(result_method);
        helper_items.append(&mut result_helpers);

        let goal_response_data_ident = Ident::new("GoalResponseData", Span::call_site());
        let data_field = has_goal_response_data.then(|| {
            quote! {
                /// The declared goal response of an accepted goal; `None`
                /// for a refused goal.
                pub data: Option<#goal_response_data_ident>,
            }
        });
        let action_handle_struct = quote! {
            pub struct ActionHandle {
                messenger: peppylib::MessengerHandle,
                inner: peppylib::messaging::ActionGoalHandle,
                /// Whether the producer accepted the goal: `refusal` is
                /// `None`.
                pub accepted: bool,
                /// The failure the producer refused the goal with, of a kind
                /// that this member declares; `None` for an accepted goal.
                pub refusal: Option<Failure>,
                #data_field
            }
        };

        let bound_producer_accessor_fn =
            crate::generator::rust::topics::build_bound_producer_accessor_fn(dependency);

        let mut items = vec![
            constants_tokens,
            bound_producer_accessor_fn,
            build_failure_items(
                &dependency.member_failure_kinds(),
                &quote!(TARGET_ACTION_NAME),
                FailingMember::Action,
            ),
        ];
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
        self.push_section(InterfaceArtifact {
            module_path: vec![action.link_id.clone(), action.name.clone()],
            kind: InterfaceKind::ConsumedAction,
            code_output: rendered,
        });
        Ok(())
    }

    fn build(
        mut self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &daemon_config::consts::PeppyDirs,
        deploy_mode: crate::generator::common::CrateDeployMode,
    ) -> Result<()> {
        // Render the test surfaces first: their codecs register on the same
        // schema keys production used (dedupe by file stem), so this must run
        // before the schema set is written.
        let registry = std::mem::take(&mut self.testgen);
        mock::render(&mut self, &registry)?;
        fixtures::render(&mut self, &registry)?;
        scaffold::add_peppylib_dependencies(&to_path, peppy_dirs, deploy_mode)?;
        scaffold::add_capnp_schemas(&self.schemas, to_path.as_ref())?;
        scaffold::add_artifacts_to_lib(&to_path, self.sections)?;
        scaffold::add_parameters_to_lib(&to_path, &self.parameters)?;
        Ok(())
    }
}

/// One `#[doc = "..."]` attribute per pre-wrapped doc line (the renderer
/// prints each attribute back as a `///` line). Splices the shared
/// per-cardinality prose of [`DependencyContext::target_selection_doc`]
/// and [`DependencyContext::bound_producers_doc`] into generated items.
///
/// [`DependencyContext::target_selection_doc`]: crate::generator::types::DependencyContext::target_selection_doc
/// [`DependencyContext::bound_producers_doc`]: crate::generator::types::DependencyContext::bound_producers_doc
fn doc_attrs(lines: &[&str]) -> Vec<TokenStream> {
    lines
        .iter()
        .map(|line| {
            let line = Literal::string(&format!(" {line}"));
            quote!(#[doc = #line])
        })
        .collect()
}
