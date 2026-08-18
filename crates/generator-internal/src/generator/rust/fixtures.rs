//! Renderer for the generated `fixtures` category: the per-node `Harness`
//! (ephemeral router + started mocks + seeded `StandaloneConfig` + running
//! node behind the readiness barriers) and typed observation clients for the
//! node's own surface (`emitted_topics` subscriptions, `exposed_services` /
//! `exposed_actions` identity-explicit callers). Lifecycle semantics live in
//! `peppylib::testing::HarnessCore`; this file contributes only what is
//! per-node: mock construction, seeding calls, readiness lists, and typed
//! codecs on the production schema keys.
//!
//! Skipped entirely when the backend was driven without
//! [`set_node_identity`](super::RustGenerator::set_node_identity) — the
//! harness cannot pin the node's own targets without the manifest identity.

use super::super::testgen::{
    DepLinkSpec, EmittedSpec, ExposedActionSpec, ExposedServiceSpec, FIXTURE_CORE_NODE,
    FIXTURE_INSTANCE_ID, TargetSpec, TestGenRegistry,
};
use super::mock::{
    build_local_message_with_deserializer, build_struct_deserializer,
    build_struct_serializer, production_module, target_expr,
};
use super::scaffold::sanitize_rust_module_name;
use super::topics::qos_profile_tokens;
use super::{
    GenerationContext, RustGenerator, build_deserialize_fn, build_serialize_payload,
    collect_function_params, deserialize_format_fields, generate_assignments_from_struct,
    map_message_format, prefixed_ident, render_tokens,
};
use crate::error::{Error, Result};
use crate::generator::naming::{non_empty_str, to_camel_case};
use crate::generator::types::{InterfaceArtifact, InterfaceKind, scoped_schema_key};
use config::node::{Cardinality, MessageFormat};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use std::collections::HashSet;

pub(super) fn render(generator: &mut RustGenerator, registry: &TestGenRegistry) -> Result<()> {
    let Some((node_name, node_tag)) = registry.node_identity.clone() else {
        return Ok(());
    };

    let mut emitted_members: Vec<EmittedMember> = Vec::new();
    let mut seen = HashSet::new();
    for spec in &registry.own.emitted {
        let (artifact, member) = render_emitted_topic(generator, &node_name, &node_tag, spec)?;
        // The field ident is origin-scoped (contract-backed topics share
        // leaf names across links), so it doubles as the uniqueness key.
        claim(&mut seen, &member.field.to_string(), "fixtures emitted", &spec.name)?;
        generator.push_section(artifact);
        emitted_members.push(member);
    }

    for spec in &registry.own.services {
        let artifact = render_exposed_service(generator, &node_name, &node_tag, spec)?;
        generator.push_section(artifact);
    }

    for spec in &registry.own.actions {
        let artifact = render_exposed_action(generator, &node_name, &node_tag, spec)?;
        generator.push_section(artifact);
    }

    let harness = render_harness(registry, &node_name, &node_tag, &emitted_members)?;
    generator.push_section(InterfaceArtifact {
        module_path: vec!["harness".to_string()],
        kind: InterfaceKind::Fixture,
        code_output: harness,
    });
    Ok(())
}

fn claim(seen: &mut HashSet<String>, sanitized: &str, category: &str, raw: &str) -> Result<()> {
    if !seen.insert(sanitized.to_string()) {
        return Err(Error::ModuleNameCollision {
            category: category.to_string(),
            first: sanitized.to_string(),
            second: raw.to_string(),
            sanitized: sanitized.to_string(),
        });
    }
    Ok(())
}

/// The node's own publish/serve identity for a surface, baked at generation
/// time: its contract target when contract-routed, else the manifest target.
fn own_target_expr(
    node_name: &str,
    node_tag: &str,
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> TokenStream {
    match origin {
        Some(origin) => target_expr(&TargetSpec::Contract {
            name: origin.contract_name.clone(),
            tag: origin.contract_tag.clone(),
        }),
        None => target_expr(&TargetSpec::Node {
            name: node_name.to_string(),
            tag: node_tag.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// emitted topics
// ---------------------------------------------------------------------------

struct EmittedMember {
    field: Ident,
    module: TokenStream,
    topic: String,
    origin_target: TokenStream,
}

fn render_emitted_topic(
    generator: &mut RustGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &EmittedSpec,
) -> Result<(InterfaceArtifact, EmittedMember)> {
    let topic_name = spec.name.as_str();
    let fn_name = prefixed_ident("", non_empty_str(topic_name), "topic").to_string();
    let schema_key = scoped_schema_key(spec.origin.as_ref(), &fn_name);
    let schema_prefix = to_camel_case(&fn_name);
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());
    let qos = qos_profile_tokens(&spec.qos);

    let deserialize_ident = Ident::new("deserialize_message", Span::call_site());
    // The emitted direction has no production consumer struct (the node-side
    // module only serializes), so the fixtures module defines its own.
    let message_and_deserializer = build_local_message_with_deserializer(
        generator,
        &schema_key,
        &schema_prefix,
        &spec.format,
        &deserialize_ident,
        &format!("fixtures observe {topic_name}"),
    )?;

    let doc = format!(
        "Typed observation of the node's emitted topic `{topic_name}`: the harness \
         subscribes before the node boots (pinned to the node's identity) and barriers \
         on the subscription, so the node's very first publish is captured."
    );
    let module = quote! {
        #![doc = #doc]

        /// A held subscription to the node's emissions on this topic.
        pub struct Subscription {
            inner: peppylib::messaging::Subscription,
        }

        pub(crate) async fn subscribe(
            session: &peppylib::MessengerHandle,
            node_instance_id: &str,
        ) -> crate::Result<Subscription> {
            let producer = peppylib::messaging::ProducerRef::new(
                peppylib::testing::STANDALONE_CORE_NODE,
                node_instance_id,
            );
            let inner = peppylib::TopicMessenger::subscribe(
                session,
                #FIXTURE_CORE_NODE,
                #FIXTURE_INSTANCE_ID,
                #target,
                #topic_name,
                &producer,
                #qos,
            )
            .await?;
            Ok(Subscription { inner })
        }

        impl Subscription {
            /// Awaits the node's next message on this topic; `Ok(None)` once
            /// the fixture session closes.
            #[allow(clippy::should_implement_trait)]
            pub async fn next(&mut self) -> crate::Result<Option<Message>> {
                let Some(message) = self.inner.on_next_message().await else {
                    return Ok(None);
                };
                let message = deserialize_message(message.payload_bytes().as_ref())?;
                Ok(Some(message))
            }
        }

        #message_and_deserializer
    };

    let mut module_path = vec!["emitted_topics".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(topic_name)),
        None => module_path.push(topic_name.to_string()),
    }
    // Contract-backed topics nest under their link_id in the tree; the flat
    // Emitted field mirrors that scoping so same-name topics from different
    // links coexist.
    let field_name = match &spec.origin {
        Some(origin) => format!("{}_{}", origin.link_id, topic_name),
        None => topic_name.to_string(),
    };
    let field = Ident::new(
        &sanitize_rust_module_name(&field_name),
        Span::call_site(),
    );
    let production_path = {
        let segments: Vec<&str> = module_path[1..].iter().map(String::as_str).collect();
        production_module("fixtures", &{
            let mut all = vec!["emitted_topics"];
            all.extend(segments);
            all
        })
    };
    Ok((
        InterfaceArtifact {
            module_path,
            kind: InterfaceKind::Fixture,
            code_output: render_tokens(module),
        },
        EmittedMember {
            field,
            module: production_path,
            topic: topic_name.to_string(),
            origin_target: own_target_expr(node_name, node_tag, spec.origin.as_ref()),
        },
    ))
}

// ---------------------------------------------------------------------------
// exposed services
// ---------------------------------------------------------------------------

fn render_exposed_service(
    generator: &mut RustGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &ExposedServiceSpec,
) -> Result<InterfaceArtifact> {
    let service_name = spec.name.as_str();
    let fn_name = prefixed_ident("", non_empty_str(service_name), "service").to_string();
    let request_key = scoped_schema_key(spec.origin.as_ref(), &fn_name);
    let response_key = scoped_schema_key(spec.origin.as_ref(), &format!("{fn_name}_response"));
    let schema_prefix = to_camel_case(&fn_name);
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());

    let mut module_path = vec!["exposed_services".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(service_name)),
        None => module_path.push(service_name.to_string()),
    }
    let production = {
        let segments: Vec<&str> = module_path[1..].iter().map(String::as_str).collect();
        let mut all = vec!["exposed_services"];
        all.extend(segments);
        production_module_raw(&all)
    };

    let serialize_ident = Ident::new("serialize_request", Span::call_site());
    let deserialize_ident = Ident::new("deserialize_response", Span::call_site());

    let mut reexports: Vec<TokenStream> = Vec::new();
    let mut codecs: Vec<TokenStream> = Vec::new();

    let (request_param, request_payload) = match &spec.request {
        Some(format) => {
            reexports.push(quote!(pub use #production::RequestData;));
            codecs.push(build_struct_serializer(
                generator,
                &serialize_ident,
                &request_key,
                &schema_prefix,
                format,
                &quote!(RequestData),
                &format!("fixtures poll {service_name}"),
            )?);
            (quote!(request: &RequestData,), quote!(serialize_request(request)?))
        }
        None => (quote!(), quote!(peppylib::Payload::new())),
    };

    let (return_ty, decode_response) = match &spec.response {
        Some(format) => {
            reexports.push(quote!(pub use #production::Response;));
            codecs.push(build_struct_deserializer(
                generator,
                &deserialize_ident,
                &response_key,
                &format!("{schema_prefix}Response"),
                format,
                &quote!(Response),
                "Response",
                &format!("fixtures poll {service_name} response"),
            )?);
            (
                quote!(Response),
                quote!(deserialize_response(response_message.payload_bytes().as_ref())),
            )
        }
        None => (
            quote!(()),
            quote! {{
                let _ = response_message;
                Ok(())
            }},
        ),
    };

    let doc = format!(
        "Identity-explicit caller for the node's exposed service `{service_name}`: polls \
         from the fixture session, pinned to the node under test, gating on reachability \
         first (the fixture session is a fresh caller)."
    );
    let module = quote! {
        #![doc = #doc]

        #( #reexports )*
        #[allow(unused_imports)]
        use #production::*;

        /// Polls the node's service once and decodes the reply. `timeout`
        /// bounds the reachability gate and the poll individually.
        pub async fn poll(
            harness: &crate::fixtures::harness::Harness,
            #request_param
            timeout: std::time::Duration,
        ) -> crate::Result<#return_ty> {
            let producer = harness.node_producer_ref();
            peppylib::testing::wait_service_reachable(
                harness.session(),
                #FIXTURE_CORE_NODE,
                #FIXTURE_INSTANCE_ID,
                #target,
                #service_name,
                &producer,
                timeout,
            )
            .await?;
            let response_message = peppylib::ServiceMessenger::poll(
                harness.session(),
                #FIXTURE_CORE_NODE,
                #FIXTURE_INSTANCE_ID,
                #target,
                #service_name,
                peppylib::messaging::ServiceTarget::Producer(&producer),
                #request_payload,
                timeout,
            )
            .await?;
            #decode_response
        }

        #( #codecs )*
    };
    Ok(InterfaceArtifact {
        module_path,
        kind: InterfaceKind::Fixture,
        code_output: render_tokens(module),
    })
}

/// Path to a production module from raw segments (first segment is the
/// category, the rest are sanitized).
fn production_module_raw(segments: &[&str]) -> TokenStream {
    production_module(segments[0], &segments[1..])
}

// ---------------------------------------------------------------------------
// exposed actions
// ---------------------------------------------------------------------------

fn render_exposed_action(
    generator: &mut RustGenerator,
    node_name: &str,
    node_tag: &str,
    spec: &ExposedActionSpec,
) -> Result<InterfaceArtifact> {
    let action_name = spec.name.as_str();
    let base_name = prefixed_ident("", non_empty_str(action_name), "action").to_string();
    let goal_key = scoped_schema_key(spec.origin.as_ref(), &format!("{base_name}_goal"));
    let goal_response_key =
        scoped_schema_key(spec.origin.as_ref(), &format!("{base_name}_goal_response"));
    let feedback_key =
        scoped_schema_key(spec.origin.as_ref(), &format!("emit_{base_name}_feedback"));
    let result_key =
        scoped_schema_key(spec.origin.as_ref(), &format!("{base_name}_result_response"));
    let action_prefix = to_camel_case(&base_name);
    let target = own_target_expr(node_name, node_tag, spec.origin.as_ref());

    let mut module_path = vec!["exposed_actions".to_string()];
    match &spec.origin {
        Some(origin) => module_path.extend(origin.module_path_for(action_name)),
        None => module_path.push(sanitize_display(action_name)),
    }
    let production = {
        let segments: Vec<&str> = module_path[1..].iter().map(String::as_str).collect();
        let mut all = vec!["exposed_actions"];
        all.extend(segments);
        production_module_raw(&all)
    };

    let mut reexports: Vec<TokenStream> = Vec::new();
    let mut codecs: Vec<TokenStream> = Vec::new();
    let mut extra_items: Vec<TokenStream> = Vec::new();

    // Goal request: fixtures serialize (production only deserializes).
    let (goal_param, goal_payload) = match &spec.goal_request {
        Some(format) => {
            reexports.push(quote!(pub use #production::GoalRequestData;));
            let fn_ident = Ident::new("serialize_goal_request", Span::call_site());
            codecs.push(build_struct_serializer(
                generator,
                &fn_ident,
                &goal_key,
                &format!("{action_prefix}Goal"),
                format,
                &quote!(GoalRequestData),
                &format!("fixtures send_goal {action_name}"),
            )?);
            (
                quote!(request: &GoalRequestData,),
                quote!(serialize_goal_request(request)?),
            )
        }
        None => (quote!(), quote!(peppylib::Payload::new())),
    };

    // Goal response: fixtures deserialize into the production `GoalResponse`.
    let (data_field, decode_goal_response, data_init) = match &spec.goal_response {
        Some(format) => {
            reexports.push(quote!(pub use #production::GoalResponse;));
            let fn_ident = Ident::new("deserialize_goal_response", Span::call_site());
            codecs.push(build_struct_deserializer(
                generator,
                &fn_ident,
                &goal_response_key,
                &format!("{action_prefix}GoalResponse"),
                format,
                &quote!(GoalResponse),
                "GoalResponse",
                &format!("fixtures {action_name} goal response"),
            )?);
            (
                quote! {
                    /// The declared goal response; `None` when a rejection
                    /// carried no payload.
                    pub data: Option<GoalResponse>,
                },
                quote! {
                    let data = if reply.body.is_empty() {
                        None
                    } else {
                        Some(deserialize_goal_response(reply.body.as_ref())?)
                    };
                },
                quote!(data,),
            )
        }
        None => (quote!(), quote!(), quote!()),
    };

    // Feedback: fixtures deserialize; production defines no struct.
    let feedback_method = match &spec.feedback {
        Some(format) => {
            let fn_ident = Ident::new("deserialize_feedback", Span::call_site());
            let artifacts = map_message_format(&feedback_key, Some(format))?
                .expect("non-empty message format maps to schema artifacts");
            let mut context = GenerationContext::default();
            let feedback_ident = Ident::new("Feedback", Span::call_site());
            let params =
                collect_function_params(Some(&artifacts), None, "Feedback", &mut context, None)?;
            let fields: Vec<(Ident, TokenStream)> = params
                .iter()
                .map(|param| (param.ident.clone(), param.ty.clone()))
                .collect();
            context.add_struct(feedback_ident, fields);
            extra_items.extend(context.into_tokens());

            let info = generator.register_schema(
                &feedback_key,
                &format!("{action_prefix}Feedback"),
                &artifacts,
            )?;
            let reader_type = info.reader_type_tokens();
            let context_expr = {
                let label = format!("fixtures {action_name} feedback");
                quote!(String::from(#label))
            };
            let (field_statements, field_inits, _) =
                deserialize_format_fields(format, "Feedback", &context_expr)?;
            let return_ty = quote!(Feedback);
            let result_expr = quote!(Feedback { #(#field_inits),* });
            codecs.push(build_deserialize_fn(
                &fn_ident,
                &reader_type,
                &context_expr,
                &return_ty,
                &field_statements,
                &result_expr,
            ));
            quote! {
                /// Receives the next decoded feedback message for this goal.
                ///
                /// Terminal errors that end a drain loop:
                /// `Error::ActionFeedbackChannelClosed` on a clean close and
                /// `Error::ActionFeedbackProducerGone` when the node instance
                /// disappeared without closing the stream.
                pub async fn on_next_feedback(&mut self) -> crate::Result<Feedback> {
                    let feedback = self.inner.on_next_feedback().await?;
                    let payload = feedback.payload_bytes();
                    deserialize_feedback(payload.as_ref())
                }
            }
        }
        None => quote!(),
    };

    // Result: fixtures deserialize; production defines no struct.
    let (outcome_variants, decode_result) = match &spec.result {
        Some(format) => {
            let fn_ident = Ident::new("deserialize_result", Span::call_site());
            let artifacts = map_message_format(&result_key, Some(format))?
                .expect("non-empty message format maps to schema artifacts");
            let mut context = GenerationContext::default();
            let result_ident = Ident::new("ResultData", Span::call_site());
            let params =
                collect_function_params(Some(&artifacts), None, "ResultData", &mut context, None)?;
            let fields: Vec<(Ident, TokenStream)> = params
                .iter()
                .map(|param| (param.ident.clone(), param.ty.clone()))
                .collect();
            context.add_struct(result_ident, fields);
            extra_items.extend(context.into_tokens());

            let info = generator.register_schema(
                &result_key,
                &format!("{action_prefix}ResultResponse"),
                &artifacts,
            )?;
            let reader_type = info.reader_type_tokens();
            let context_expr = {
                let label = format!("fixtures {action_name} result");
                quote!(String::from(#label))
            };
            let (field_statements, field_inits, _) =
                deserialize_format_fields(format, "ResultData", &context_expr)?;
            let return_ty = quote!(ResultData);
            let result_expr = quote!(ResultData { #(#field_inits),* });
            codecs.push(build_deserialize_fn(
                &fn_ident,
                &reader_type,
                &context_expr,
                &return_ty,
                &field_statements,
                &result_expr,
            ));
            (
                quote! {
                    Completed(ResultData),
                    Cancelled(ResultData),
                    Abandoned,
                    Expired,
                },
                quote! {
                    let outcome = match reply.status {
                        peppylib::messaging::ResultStatus::Completed => {
                            ResultOutcome::Completed(deserialize_result(reply.body.as_ref())?)
                        }
                        peppylib::messaging::ResultStatus::Cancelled => {
                            ResultOutcome::Cancelled(deserialize_result(reply.body.as_ref())?)
                        }
                        peppylib::messaging::ResultStatus::Abandoned => ResultOutcome::Abandoned,
                        peppylib::messaging::ResultStatus::Expired => ResultOutcome::Expired,
                    };
                },
            )
        }
        None => (
            quote! {
                Completed,
                Cancelled,
                Abandoned,
                Expired,
            },
            quote! {
                let outcome = match reply.status {
                    peppylib::messaging::ResultStatus::Completed => ResultOutcome::Completed,
                    peppylib::messaging::ResultStatus::Cancelled => ResultOutcome::Cancelled,
                    peppylib::messaging::ResultStatus::Abandoned => ResultOutcome::Abandoned,
                    peppylib::messaging::ResultStatus::Expired => ResultOutcome::Expired,
                };
            },
        ),
    };

    let doc = format!(
        "Identity-explicit caller for the node's exposed action `{action_name}`: drives \
         the full goal lifecycle from the fixture session, pinned to the node under \
         test, gating on reachability first."
    );
    let module = quote! {
        #![doc = #doc]

        #( #reexports )*
        #[allow(unused_imports)]
        use #production::*;

        #( #extra_items )*

        /// A goal in flight against the node's action.
        pub struct GoalHandle {
            messenger: peppylib::MessengerHandle,
            inner: peppylib::messaging::ActionGoalHandle,
            /// Whether the node admitted the goal.
            pub accepted: bool,
            /// Optional human-readable rejection reason.
            pub reason: Option<String>,
            #data_field
        }

        /// Sends a goal to the node and decodes the admission reply.
        pub async fn send_goal(
            harness: &crate::fixtures::harness::Harness,
            #goal_param
            feedback_qos: peppylib::config::QoSProfile,
            timeout: std::time::Duration,
        ) -> crate::Result<GoalHandle> {
            let producer = harness.node_producer_ref();
            peppylib::testing::wait_action_reachable(
                harness.session(),
                #FIXTURE_CORE_NODE,
                #FIXTURE_INSTANCE_ID,
                #target,
                #action_name,
                &producer,
                timeout,
            )
            .await?;
            let inner = peppylib::ActionMessenger::send_goal(
                harness.session(),
                #FIXTURE_CORE_NODE,
                #FIXTURE_INSTANCE_ID,
                #target,
                #action_name,
                Some(&producer),
                #goal_payload,
                feedback_qos,
                timeout,
            )
            .await?;
            let reply = inner.goal_reply();
            let accepted = reply.accepted;
            let reason = reply.reason.clone();
            #decode_goal_response
            Ok(GoalHandle {
                messenger: harness.session().clone(),
                inner,
                accepted,
                reason,
                #data_init
            })
        }

        /// How a finished goal ended.
        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub enum ResultOutcome {
            #outcome_variants
        }

        /// The decoded result reply.
        #[derive(Debug, Clone)]
        #[allow(dead_code)]
        pub struct ResultResponse {
            pub instance_id: String,
            pub core_node: String,
            pub outcome: ResultOutcome,
        }

        /// Cancel acknowledgement states, mirroring the runtime's.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(dead_code)]
        pub enum CancelState {
            Signalled,
            AlreadyTerminal,
            Unknown,
        }

        impl GoalHandle {
            #feedback_method

            /// Requests cancellation of this goal.
            pub async fn cancel_goal(
                &self,
                timeout: std::time::Duration,
            ) -> crate::Result<CancelState> {
                let response = peppylib::ActionMessenger::cancel_goal(
                    &self.messenger,
                    &self.inner,
                    timeout,
                )
                .await?;
                let state = match peppylib::messaging::decode_cancel_ack(
                    response.payload_bytes().as_ref(),
                )? {
                    peppylib::messaging::CancelState::Signalled => CancelState::Signalled,
                    peppylib::messaging::CancelState::AlreadyTerminal => {
                        CancelState::AlreadyTerminal
                    }
                    peppylib::messaging::CancelState::Unknown => CancelState::Unknown,
                };
                Ok(state)
            }

            /// Retrieves the goal's terminal result.
            pub async fn get_result(
                &self,
                timeout: std::time::Duration,
            ) -> crate::Result<ResultResponse> {
                let reply = peppylib::ActionMessenger::request_result(
                    &self.messenger,
                    &self.inner,
                    timeout,
                )
                .await?;
                #decode_result
                Ok(ResultResponse {
                    instance_id: reply.instance_id,
                    core_node: reply.core_node,
                    outcome,
                })
            }
        }

        #( #codecs )*
    };
    Ok(InterfaceArtifact {
        module_path,
        kind: InterfaceKind::Fixture,
        code_output: render_tokens(module),
    })
}

fn sanitize_display(name: &str) -> String {
    crate::generator::naming::sanitize_node_display_name(name)
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn render_harness(
    registry: &TestGenRegistry,
    node_name: &str,
    node_tag: &str,
    emitted_members: &[EmittedMember],
) -> Result<String> {
    let mut config_fields: Vec<TokenStream> = Vec::new();
    let mut config_defaults: Vec<TokenStream> = Vec::new();
    let mut dep_mock_fields: Vec<TokenStream> = Vec::new();
    let mut pairing_mock_fields: Vec<TokenStream> = Vec::new();
    let mut observed_mock_fields: Vec<TokenStream> = Vec::new();
    let mut mock_starts: Vec<TokenStream> = Vec::new();
    let mut dep_mock_inits: Vec<Ident> = Vec::new();
    let mut pairing_mock_inits: Vec<Ident> = Vec::new();
    let mut observed_mock_inits: Vec<Ident> = Vec::new();
    let mut seeding: Vec<TokenStream> = Vec::new();
    let mut publisher_readiness: Vec<TokenStream> = Vec::new();
    let mut service_readiness: Vec<TokenStream> = Vec::new();

    for (link_id, spec) in &registry.deps {
        render_dep_harness_parts(
            link_id,
            spec,
            &mut config_fields,
            &mut config_defaults,
            &mut dep_mock_fields,
            &mut mock_starts,
            &mut dep_mock_inits,
            &mut seeding,
            &mut service_readiness,
        );
    }

    for (link_id, spec) in &registry.pairings {
        let field = Ident::new(&sanitize_rust_module_name(link_id), Span::call_site());
        let module = production_module("mock", &["pairings", link_id]);
        pairing_mock_fields.push(quote!(pub #field: #module::Mock));
        mock_starts.push(quote! {
            let #field = #module::Mock::start(&router, &instance_id).await?;
        });
        pairing_mock_inits.push(field.clone());
        let seed_pin = quote! {
            standalone = standalone.with_peer_pin(
                #link_id,
                #module::MOCK_CORE_NODE,
                #module::MOCK_INSTANCE_ID,
                #module::PEER_LINK_ID,
            );
        };
        if spec.optional {
            // The mock still starts when vacant: its pinned subscription is
            // what resolves the publisher-readiness barrier, and an unpaired
            // node's publishes on the slot are legal no-ops — so only the
            // pin seeding is withheld.
            let vacant_field = Ident::new(&format!("{field}_vacant"), Span::call_site());
            let vacant_doc = format!(
                "Boot with the optional `{link_id}` pairing slot unpaired: \
                 the peer pin is not seeded, so the node's `paired()` stays \
                 false, its publishes on this slot are no-ops, and the \
                 still-started mock's subscriptions stay silent."
            );
            config_fields.push(quote! {
                #[doc = #vacant_doc]
                pub #vacant_field: bool
            });
            config_defaults.push(quote!(#vacant_field: false));
            seeding.push(quote! {
                if !config.#vacant_field {
                    #seed_pin
                }
            });
        } else {
            seeding.push(seed_pin);
        }
        // The mock subscribes to every topic the node emits on this slot;
        // barrier on each so the node's first publish routes.
        for topic in &spec.node_emits {
            let topic_name = topic.name.as_str();
            let pairing_target = target_expr(&TargetSpec::Pairing {
                name: spec.pairing_name.clone(),
                tag: spec.pairing_tag.clone(),
            });
            publisher_readiness.push(quote! {
                peppylib::testing::PublisherReadiness {
                    target: #pairing_target,
                    link_id: Some(#link_id.to_string()),
                    topic: #topic_name.to_string(),
                },
            });
        }
    }

    for (link_id, spec) in &registry.observed {
        let field = Ident::new(&sanitize_rust_module_name(link_id), Span::call_site());
        let module = production_module("mock", &["observed", link_id]);
        match spec.cardinality {
            Cardinality::One => {
                observed_mock_fields.push(quote!(pub #field: #module::Mock));
                mock_starts.push(quote! {
                    let #field = #module::Mock::start(&router).await?;
                });
                seeding.push(quote! {
                    standalone = standalone.with_observed_source(
                        #link_id,
                        #module::MOCK_CORE_NODE,
                        #module::MOCK_INSTANCE_ID,
                        #module::SOURCE_LINK_ID,
                    );
                });
                observed_mock_inits.push(field);
            }
            Cardinality::ZeroOrOne => {
                let vacant_field =
                    Ident::new(&format!("{field}_vacant"), Span::call_site());
                let vacant_doc = format!(
                    "Leave the `{link_id}` observer slot empty (its cardinality \
                     admits an empty set)."
                );
                config_fields.push(quote! {
                    #[doc = #vacant_doc]
                    pub #vacant_field: bool
                });
                config_defaults.push(quote!(#vacant_field: false));
                observed_mock_fields.push(quote!(pub #field: Option<#module::Mock>));
                mock_starts.push(quote! {
                    let #field = if config.#vacant_field {
                        None
                    } else {
                        Some(#module::Mock::start(&router).await?)
                    };
                });
                seeding.push(quote! {
                    if !config.#vacant_field {
                        standalone = standalone.with_observed_source(
                            #link_id,
                            #module::MOCK_CORE_NODE,
                            #module::MOCK_INSTANCE_ID,
                            #module::SOURCE_LINK_ID,
                        );
                    }
                });
                observed_mock_inits.push(field);
            }
            Cardinality::OneOrMore | Cardinality::ZeroOrMore => {
                let count_field =
                    Ident::new(&format!("{field}_instances"), Span::call_site());
                let default_count: usize = match spec.cardinality {
                    Cardinality::OneOrMore => 1,
                    _ => 0,
                };
                let count_doc = format!(
                    "How many mock sources to start for the `{link_id}` observer slot."
                );
                let ids_field =
                    Ident::new(&format!("{field}_instance_ids"), Span::call_site());
                let ids_doc = format!(
                    "Explicit instance ids for the `{link_id}` mock sources, \
                     overriding `{count_field}` when non-empty. For nodes that \
                     classify sources by instance name."
                );
                let ids_local =
                    Ident::new(&format!("{field}_member_ids"), Span::call_site());
                config_fields.push(quote! {
                    #[doc = #count_doc]
                    pub #count_field: usize
                });
                config_fields.push(quote! {
                    #[doc = #ids_doc]
                    pub #ids_field: Vec<String>
                });
                config_defaults.push(quote!(#count_field: #default_count));
                config_defaults.push(quote!(#ids_field: Vec::new()));
                observed_mock_fields.push(quote!(pub #field: Vec<#module::Mock>));
                mock_starts.push(quote! {
                    let #ids_local: Vec<String> = if config.#ids_field.is_empty() {
                        (0..config.#count_field)
                            .map(|index| format!("{}-{}", #module::MOCK_INSTANCE_ID, index))
                            .collect()
                    } else {
                        config.#ids_field.clone()
                    };
                    let mut #field = Vec::new();
                    for member_instance_id in &#ids_local {
                        #field.push(
                            #module::Mock::start_as(&router, member_instance_id).await?,
                        );
                    }
                });
                seeding.push(quote! {
                    for member_instance_id in &#ids_local {
                        standalone = standalone.with_observed_source(
                            #link_id,
                            #module::MOCK_CORE_NODE,
                            member_instance_id.clone(),
                            #module::SOURCE_LINK_ID,
                        );
                    }
                });
                observed_mock_inits.push(field);
            }
        }
    }

    // Emitted-topic subscriptions: opened on the fixture session before the
    // node boots, then barriered on.
    let mut emitted_fields: Vec<TokenStream> = Vec::new();
    let mut emitted_subscribes: Vec<TokenStream> = Vec::new();
    let mut emitted_inits: Vec<&Ident> = Vec::new();
    for member in emitted_members {
        let field = &member.field;
        let module = &member.module;
        let topic = member.topic.as_str();
        let target = &member.origin_target;
        emitted_fields.push(quote!(pub #field: #module::Subscription));
        emitted_subscribes.push(quote! {
            let #field = #module::subscribe(&session, &instance_id).await?;
        });
        emitted_inits.push(field);
        publisher_readiness.push(quote! {
            peppylib::testing::PublisherReadiness {
                target: #target,
                link_id: None,
                topic: #topic.to_string(),
            },
        });
    }

    let parameters_seed = quote! {
        if let Some(parameters) = &config.parameters {
            standalone = standalone.with_parameters(parameters);
        }
    };

    let doc = "Generated test harness: ephemeral router + started mocks + seeded \
               `StandaloneConfig` + the node running in-process behind the readiness \
               barriers. Construct with [`Harness::start`] (or [`Harness::start_with`] \
               for parameter/slot overrides); tear down with [`Harness::shutdown`].";
    let tokens = quote! {
        #![doc = #doc]

        /// Wire identity of the fixture caller/observer session.
        pub const FIXTURE_CORE_NODE: &str = #FIXTURE_CORE_NODE;
        pub const FIXTURE_INSTANCE_ID: &str = #FIXTURE_INSTANCE_ID;
        /// The node's manifest identity, baked at sync time.
        pub const NODE_NAME: &str = #node_name;
        pub const NODE_TAG: &str = #node_tag;

        /// The node's `peppy.json5`, resolved relative to the generated
        /// crate's manifest (always `<node>/.peppy/libs/peppygen`).
        pub const PEPPY_CONFIG_PATH: &str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../../peppy.json5");

        /// Optional overrides for [`Harness::start_with`].
        pub struct Config {
            /// Typed parameters for the node; `None` uses the schema
            /// defaults (an error at boot if a required parameter has none).
            pub parameters: Option<crate::Parameters>,
            /// Explicit node instance id; `None` generates a unique one.
            pub instance_id: Option<String>,
            #( #config_fields ),*
        }

        // Not derivable in general: multi-instance slot counts default
        // per-cardinality (1 for one_or_more).
        #[allow(clippy::derivable_impls)]
        impl Default for Config {
            fn default() -> Self {
                Self {
                    parameters: None,
                    instance_id: None,
                    #( #config_defaults ),*
                }
            }
        }

        /// Every started mock, one field per link. Returned alongside the
        /// harness so a test can consume an individual mock (`stop()` is
        /// producer-loss) without giving up the harness.
        pub struct Mocks {
            pub deps: DepMocks,
            pub pairings: PairingMocks,
            pub observed: ObservedMocks,
        }

        pub struct DepMocks {
            #( #dep_mock_fields ),*
        }

        pub struct PairingMocks {
            #( #pairing_mock_fields ),*
        }

        pub struct ObservedMocks {
            #( #observed_mock_fields ),*
        }

        /// Typed subscriptions to the node's emitted topics, opened before
        /// the node booted (so its very first publish is captured).
        pub struct Emitted {
            #( #emitted_fields ),*
        }

        pub struct Harness {
            core: peppylib::testing::HarnessCore,
            /// Observation subscriptions to the node's own emissions.
            pub emitted: Emitted,
            session: peppylib::MessengerHandle,
            router: Option<peppylib::testing::EphemeralRouter>,
            instance_id: String,
        }

        impl Harness {
            /// Boots the full fixture under default configuration. `setup`
            /// is the node's real entry point (the exact closure shape
            /// `NodeBuilder::run` takes), spawned only after every readiness
            /// barrier passed.
            pub async fn start<F, Fut>(setup: F) -> crate::Result<(Self, Mocks)>
            where
                F: FnOnce(crate::Parameters, std::sync::Arc<crate::NodeRunner>) -> Fut,
                Fut: std::future::Future<Output = crate::Result<()>> + Send + 'static,
            {
                Self::start_with(Config::default(), setup).await
            }

            /// [`Harness::start`] with parameter / slot overrides.
            // vec_init_then_push: the readiness list is assembled by
            // generated per-slot statements (conditionals and loops among
            // them), not a fixed literal.
            #[allow(clippy::vec_init_then_push)]
            pub async fn start_with<F, Fut>(
                config: Config,
                setup: F,
            ) -> crate::Result<(Self, Mocks)>
            where
                F: FnOnce(crate::Parameters, std::sync::Arc<crate::NodeRunner>) -> Fut,
                Fut: std::future::Future<Output = crate::Result<()>> + Send + 'static,
            {
                let router = peppylib::testing::EphemeralRouter::start().await?;
                let instance_id = config
                    .instance_id
                    .clone()
                    .unwrap_or_else(peppylib::testing::unique_test_instance_id);

                #( #mock_starts )*

                let session = router.connect().await?;
                #( #emitted_subscribes )*

                #[allow(unused_mut)]
                let mut standalone = peppylib::runtime::StandaloneConfig::new()
                    .with_messaging(router.host(), router.port())
                    .with_instance_id(instance_id.clone());
                #parameters_seed
                #( #seeding )*

                let publisher_readiness = vec![
                    #( #publisher_readiness )*
                ];
                #[allow(unused_mut)]
                let mut service_readiness: Vec<peppylib::testing::ServiceReadiness> =
                    Vec::new();
                #( #service_readiness )*

                let core = peppylib::testing::HarnessCore::start::<crate::Parameters, _, _>(
                    PEPPY_CONFIG_PATH,
                    standalone,
                    &publisher_readiness,
                    &service_readiness,
                    setup,
                )
                .await?;

                let harness = Harness {
                    core,
                    emitted: Emitted {
                        #( #emitted_inits ),*
                    },
                    session,
                    router: Some(router),
                    instance_id,
                };
                let mocks = Mocks {
                    deps: DepMocks {
                        #( #dep_mock_inits ),*
                    },
                    pairings: PairingMocks {
                        #( #pairing_mock_inits ),*
                    },
                    observed: ObservedMocks {
                        #( #observed_mock_inits ),*
                    },
                };
                Ok((harness, mocks))
            }

            /// The running node, for calling its runtime surface directly.
            pub fn node_runner(&self) -> &std::sync::Arc<crate::NodeRunner> {
                self.core.node_runner()
            }

            /// The fixture caller/observer session (not the node's).
            pub fn session(&self) -> &peppylib::MessengerHandle {
                &self.session
            }

            /// The node-under-test's instance id.
            pub fn instance_id(&self) -> &str {
                &self.instance_id
            }

            /// The node-under-test's wire identity.
            pub fn node_producer_ref(&self) -> peppylib::messaging::ProducerRef {
                peppylib::messaging::ProducerRef::new(
                    self.core.bound_core_node(),
                    &self.instance_id,
                )
            }

            /// Whether the node's `setup` has returned.
            pub fn setup_finished(&self) -> bool {
                self.core.setup_finished()
            }

            /// Tears the fixture down in lifecycle order: node convergence
            /// (cancel → bounded setup await → shutdown hooks, propagating a
            /// setup error), then the fixture session, then the router.
            pub async fn shutdown(self) -> crate::Result<()> {
                let Harness {
                    core,
                    emitted,
                    session,
                    router,
                    ..
                } = self;
                let result = core.shutdown().await;
                drop(emitted);
                drop(session);
                if let Some(router) = router {
                    router.shutdown().await?;
                }
                result
            }
        }
    };
    Ok(render_tokens(tokens))
}

#[allow(clippy::too_many_arguments)]
fn render_dep_harness_parts(
    link_id: &str,
    spec: &DepLinkSpec,
    config_fields: &mut Vec<TokenStream>,
    config_defaults: &mut Vec<TokenStream>,
    dep_mock_fields: &mut Vec<TokenStream>,
    mock_starts: &mut Vec<TokenStream>,
    dep_mock_inits: &mut Vec<Ident>,
    seeding: &mut Vec<TokenStream>,
    service_readiness: &mut Vec<TokenStream>,
) {
    let field = Ident::new(&sanitize_rust_module_name(link_id), Span::call_site());
    let module = production_module("mock", &["deps", link_id]);
    let target = target_expr(&spec.target);

    // One ServiceReadiness entry per served interface per mock instance,
    // probed from the node's session before its setup runs; services first,
    // then actions, mirroring the mock's member layout.
    let per_instance_readiness = |instance_expr: TokenStream| -> Vec<TokenStream> {
        spec.services
            .iter()
            .map(|service| {
                (
                    service.name.as_str(),
                    quote!(peppylib::testing::ServiceReadinessKind::Service),
                )
            })
            .chain(spec.actions.iter().map(|action| {
                (
                    action.name.as_str(),
                    quote!(peppylib::testing::ServiceReadinessKind::Action),
                )
            }))
            .map(|(name, kind)| {
                let target = target.clone();
                let instance_expr = instance_expr.clone();
                quote! {
                    service_readiness.push(peppylib::testing::ServiceReadiness {
                        target: #target,
                        name: #name.to_string(),
                        producer: #module::producer_ref_for(#instance_expr),
                        kind: #kind,
                    });
                }
            })
            .collect()
    };

    match spec.cardinality {
        Cardinality::One => {
            dep_mock_fields.push(quote!(pub #field: #module::Mock));
            mock_starts.push(quote! {
                let #field = #module::Mock::start(&router).await?;
            });
            seeding.push(quote! {
                standalone = standalone.with_bound_producer(
                    #link_id,
                    #module::MOCK_CORE_NODE,
                    #module::MOCK_INSTANCE_ID,
                );
            });
            let entries = per_instance_readiness(quote!(#module::MOCK_INSTANCE_ID));
            service_readiness.push(quote! { #( #entries )* });
            dep_mock_inits.push(field);
        }
        Cardinality::ZeroOrOne => {
            let vacant_field = Ident::new(&format!("{field}_vacant"), Span::call_site());
            let vacant_doc = format!(
                "Leave the `{link_id}` dependency slot vacant (its cardinality admits \
                 an empty binding)."
            );
            config_fields.push(quote! {
                #[doc = #vacant_doc]
                pub #vacant_field: bool
            });
            config_defaults.push(quote!(#vacant_field: false));
            dep_mock_fields.push(quote!(pub #field: Option<#module::Mock>));
            mock_starts.push(quote! {
                let #field = if config.#vacant_field {
                    None
                } else {
                    Some(#module::Mock::start(&router).await?)
                };
            });
            let entries = per_instance_readiness(quote!(#module::MOCK_INSTANCE_ID));
            seeding.push(quote! {
                if config.#vacant_field {
                    standalone = standalone.with_vacant_producer_slot(#link_id);
                } else {
                    standalone = standalone.with_bound_producer(
                        #link_id,
                        #module::MOCK_CORE_NODE,
                        #module::MOCK_INSTANCE_ID,
                    );
                }
            });
            service_readiness.push(quote! {
                if !config.#vacant_field {
                    #( #entries )*
                }
            });
            dep_mock_inits.push(field);
        }
        Cardinality::OneOrMore | Cardinality::ZeroOrMore => {
            let count_field = Ident::new(&format!("{field}_instances"), Span::call_site());
            let default_count: usize = match spec.cardinality {
                Cardinality::OneOrMore => 1,
                _ => 0,
            };
            let count_doc = format!(
                "How many mock producer instances to start and bind for the \
                 `{link_id}` dependency slot."
            );
            let ids_field = Ident::new(&format!("{field}_instance_ids"), Span::call_site());
            let ids_doc = format!(
                "Explicit instance ids for the `{link_id}` mock producers, \
                 overriding `{count_field}` when non-empty. For nodes that \
                 classify producers by instance name."
            );
            let ids_local = Ident::new(&format!("{field}_member_ids"), Span::call_site());
            config_fields.push(quote! {
                #[doc = #count_doc]
                pub #count_field: usize
            });
            config_fields.push(quote! {
                #[doc = #ids_doc]
                pub #ids_field: Vec<String>
            });
            config_defaults.push(quote!(#count_field: #default_count));
            config_defaults.push(quote!(#ids_field: Vec::new()));
            dep_mock_fields.push(quote!(pub #field: Vec<#module::Mock>));
            mock_starts.push(quote! {
                let #ids_local: Vec<String> = if config.#ids_field.is_empty() {
                    (0..config.#count_field)
                        .map(|index| format!("{}-{}", #module::MOCK_INSTANCE_ID, index))
                        .collect()
                } else {
                    config.#ids_field.clone()
                };
                let mut #field = Vec::new();
                for member_instance_id in &#ids_local {
                    #field.push(#module::Mock::start_as(&router, member_instance_id).await?);
                }
            });
            let entries = per_instance_readiness(quote!(member_instance_id));
            seeding.push(quote! {
                for member_instance_id in &#ids_local {
                    standalone = standalone.with_bound_producer(
                        #link_id,
                        #module::MOCK_CORE_NODE,
                        member_instance_id.clone(),
                    );
                }
            });
            if !entries.is_empty() {
                service_readiness.push(quote! {
                    for member_instance_id in &#ids_local {
                        let member_instance_id = member_instance_id.as_str();
                        #( #entries )*
                    }
                });
            }
            dep_mock_inits.push(field);
        }
    }
}
