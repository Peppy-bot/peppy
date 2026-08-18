//! Renderer for the generated `mock` category: one typed `Mock` per link
//! under `mock::{deps,pairings,observed}::<link_id>`, playing the dependency
//! (or pairing peer / observed source) over the real wire under the exact
//! identity the harness seeds. Everything node-invariant (readiness waits,
//! scripting queues, the action engine, producer-loss ordering) lives in
//! `peppylib::testing`; this file emits only the typed veneer: identity
//! consts, per-interface sub-modules reusing the production structs, and the
//! missing-direction codecs on the production schema keys (so the capnp
//! schema set gains zero files).

use super::super::testgen::{
    DepActionSpec, DepLinkSpec, DepServiceSpec, DepTopicSpec, MOCK_CORE_NODE, MOCK_PEER_LINK_ID,
    MOCK_SOURCE_LINK_ID, ObservedLinkSpec, PairTopicSpec, PairingLinkSpec, TargetSpec,
    TestGenRegistry, mock_instance_id,
};
use super::scaffold::sanitize_rust_module_name;
use super::topics::qos_profile_tokens;
use super::{
    GenerationContext, RustGenerator, build_deserialize_fn, build_serialize_payload,
    collect_function_params, deserialize_format_fields, generate_assignments_from_struct,
    identifiers, map_message_format, render_tokens,
};
use crate::error::{Error, Result};
use crate::generator::naming::to_camel_case;
use crate::generator::types::{InterfaceArtifact, InterfaceKind};
use config::node::{MessageFormat, QoSProfile};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use std::collections::HashSet;

pub(super) fn render(generator: &mut RustGenerator, registry: &TestGenRegistry) -> Result<()> {
    for (link_id, spec) in &registry.deps {
        let code = render_dep_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["deps".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    for (link_id, spec) in &registry.pairings {
        let code = render_pairing_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["pairings".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    for (link_id, spec) in &registry.observed {
        let code = render_observed_link(generator, link_id, spec)?;
        generator.push_section(InterfaceArtifact {
            module_path: vec!["observed".to_string(), link_id.clone()],
            kind: InterfaceKind::Mock,
            code_output: code,
        });
    }
    Ok(())
}

/// `SenderTarget` expression for the identity a mock impersonates. Expands
/// with a trailing `?`, so it must appear inside a `Result` function.
pub(super) fn target_expr(target: &TargetSpec) -> TokenStream {
    match target {
        TargetSpec::Node { name, tag } => {
            quote!(peppylib::messaging::SenderTarget::node(#name, #tag)?)
        }
        TargetSpec::Contract { name, tag } => {
            quote!(peppylib::messaging::SenderTarget::contract(#name, #tag)?)
        }
        TargetSpec::Pairing { name, tag } => {
            quote!(peppylib::messaging::SenderTarget::pairing(#name, #tag)?)
        }
    }
}

/// Path to a production generated module: `crate::<category>::<segments…>`,
/// each segment sanitized exactly like the tree writer sanitizes the on-disk
/// module names.
pub(super) fn production_module(category: &str, segments: &[&str]) -> TokenStream {
    let category_ident = Ident::new(category, Span::call_site());
    let segment_idents: Vec<Ident> = segments
        .iter()
        .map(|segment| Ident::new(&sanitize_rust_module_name(segment), Span::call_site()))
        .collect();
    quote!(crate::#category_ident #(::#segment_idents)*)
}

/// Claims `raw` as a unique member name (module + field) inside one link
/// mock; a collision (e.g. a topic and a service sharing a name on one link)
/// is a hard error naming both, mirroring the scaffold's collision policy.
fn claim_member_name(
    link_id: &str,
    raw: &str,
    seen: &mut HashSet<String>,
    first_owner: &mut std::collections::HashMap<String, String>,
) -> Result<Ident> {
    let sanitized = sanitize_rust_module_name(raw);
    if !seen.insert(sanitized.clone()) {
        return Err(Error::ModuleNameCollision {
            category: "mock".to_string(),
            first: first_owner
                .get(&sanitized)
                .cloned()
                .unwrap_or_else(|| raw.to_string()),
            second: format!("{link_id}/{raw}"),
            sanitized,
        });
    }
    first_owner.insert(sanitized.clone(), format!("{link_id}/{raw}"));
    Ok(Ident::new(&sanitized, Span::call_site()))
}


/// The member name for a dep-link interface: plain when the interface's own
/// consumer-side link_id equals the dependency slot's, link-qualified
/// otherwise (a slot can bind several same-name interfaces through distinct
/// manifest entries).
fn dep_member_name(dep_link_id: &str, module_link: &str, name: &str) -> String {
    if module_link == dep_link_id {
        name.to_string()
    } else {
        format!("{module_link}_{name}")
    }
}

/// A `fn #fn_ident(value: &#ty) -> crate::Result<peppylib::Payload>`
/// serializer over an existing struct, registered on `schema_key` (dedupes
/// onto the production schema file).
fn build_struct_serializer(
    generator: &mut RustGenerator,
    fn_ident: &Ident,
    schema_key: &str,
    struct_prefix: &str,
    format: &MessageFormat,
    value_ty: &TokenStream,
    context_label: &str,
) -> Result<TokenStream> {
    let artifacts = map_message_format(schema_key, Some(format))?
        .expect("non-empty message format maps to schema artifacts");
    let info = generator.register_schema(schema_key, struct_prefix, &artifacts)?;
    let builder_type = info.builder_type_tokens();
    let builder_ident = Ident::new("root", Span::call_site());
    let value_ident = Ident::new("value", Span::call_site());
    let assignments = generate_assignments_from_struct(&builder_ident, format, &value_ident)?;
    let error_context = quote!(String::from(#context_label));
    let body = build_serialize_payload(&builder_type, &[], &assignments, &error_context);
    Ok(quote! {
        fn #fn_ident(value: &#value_ty) -> crate::Result<peppylib::Payload> {
            Ok(#body)
        }
    })
}

/// A `fn #fn_ident(payload: &[u8]) -> crate::Result<#return_ty>` deserializer
/// constructing `#return_ty { … }`. `nested_prefix` must match the prefix the
/// module defining `#return_ty` used for its nested companion structs, so the
/// emitted references resolve (the veneer glob-imports that module).
fn build_struct_deserializer(
    generator: &mut RustGenerator,
    fn_ident: &Ident,
    schema_key: &str,
    struct_prefix: &str,
    format: &MessageFormat,
    return_ty: &TokenStream,
    nested_prefix: &str,
    context_label: &str,
) -> Result<TokenStream> {
    let artifacts = map_message_format(schema_key, Some(format))?
        .expect("non-empty message format maps to schema artifacts");
    let info = generator.register_schema(schema_key, struct_prefix, &artifacts)?;
    let reader_type = info.reader_type_tokens();
    let context_expr = quote!(String::from(#context_label));
    let (field_statements, field_inits, _) =
        deserialize_format_fields(format, nested_prefix, &context_expr)?;
    let result_expr = quote!(#return_ty { #(#field_inits),* });
    Ok(build_deserialize_fn(
        fn_ident,
        &reader_type,
        &context_expr,
        return_ty,
        &field_statements,
        &result_expr,
    ))
}

/// A locally-defined `Message` struct (plus nested companions) and its
/// deserializer, for directions where production defines no struct (topics
/// the node emits: the producer side only serializes).
fn build_local_message_with_deserializer(
    generator: &mut RustGenerator,
    schema_key: &str,
    struct_prefix: &str,
    format: &MessageFormat,
    fn_ident: &Ident,
    context_label: &str,
) -> Result<TokenStream> {
    let artifacts = map_message_format(schema_key, Some(format))?
        .expect("non-empty message format maps to schema artifacts");
    let mut context = GenerationContext::default();
    let message_ident = Ident::new("Message", Span::call_site());
    let params = collect_function_params(
        Some(&artifacts),
        None,
        "Message",
        &mut context,
        None,
    )?;
    let fields: Vec<(Ident, TokenStream)> = params
        .iter()
        .map(|param| (param.ident.clone(), param.ty.clone()))
        .collect();
    context.add_struct(message_ident, fields);
    let struct_tokens = context.into_tokens();

    let info = generator.register_schema(schema_key, struct_prefix, &artifacts)?;
    let reader_type = info.reader_type_tokens();
    let context_expr = quote!(String::from(#context_label));
    let (field_statements, field_inits, _) =
        deserialize_format_fields(format, "Message", &context_expr)?;
    let return_ty = quote!(Message);
    let result_expr = quote!(Message { #(#field_inits),* });
    let deserializer = build_deserialize_fn(
        fn_ident,
        &reader_type,
        &context_expr,
        &return_ty,
        &field_statements,
        &result_expr,
    );
    Ok(quote! {
        #( #struct_tokens )*
        #deserializer
    })
}

pub(super) fn qos_tokens(qos: &QoSProfile) -> TokenStream {
    qos_profile_tokens(qos)
}

// ---------------------------------------------------------------------------
// deps
// ---------------------------------------------------------------------------

struct MemberModule {
    module: TokenStream,
    module_ident: Ident,
    field_ty: TokenStream,
    construct: TokenStream,
}

fn render_dep_link(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &DepLinkSpec,
) -> Result<String> {
    let mut seen = HashSet::new();
    let mut owners = std::collections::HashMap::new();
    let mut members: Vec<MemberModule> = Vec::new();
    let target = target_expr(&spec.target);

    for topic in &spec.topics {
        let member = dep_member_name(link_id, &topic.module_link, &topic.name);
        let module_ident = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_topic(
            generator,
            link_id,
            &spec.producer_name,
            topic,
            &target,
            module_ident,
        )?);
    }
    for service in &spec.services {
        let member = dep_member_name(link_id, &service.module_link, &service.name);
        let module_ident = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_service(
            generator,
            link_id,
            &spec.producer_name,
            service,
            &target,
            module_ident,
        )?);
    }
    for action in &spec.actions {
        let member = dep_member_name(link_id, &action.module_link, &action.name);
        let module_ident = claim_member_name(link_id, &member, &mut seen, &mut owners)?;
        members.push(render_dep_action(
            generator,
            link_id,
            &spec.producer_name,
            action,
            &target,
            module_ident,
        )?);
    }

    let default_instance = mock_instance_id(link_id);
    let modules: Vec<&TokenStream> = members.iter().map(|member| &member.module).collect();
    let fields: Vec<TokenStream> = members
        .iter()
        .map(|member| {
            let name = &member.module_ident;
            let ty = &member.field_ty;
            quote!(pub #name: #ty)
        })
        .collect();
    let constructs: Vec<&TokenStream> = members.iter().map(|member| &member.construct).collect();
    let field_names: Vec<&Ident> = members.iter().map(|member| &member.module_ident).collect();
    let action_stops: Vec<TokenStream> = spec
        .actions
        .iter()
        .map(|action| {
            let member = dep_member_name(link_id, &action.module_link, &action.name);
            let ident = Ident::new(&sanitize_rust_module_name(&member), Span::call_site());
            quote!(self.#ident.stop();)
        })
        .collect();

    let doc = format!(
        "Mock producer for the `{link_id}` dependency slot: plays the dependency over the \
         real wire under the identity the harness seeds into the slot's bound set. One \
         messaging session per mock, so [`Mock::stop`] is a whole-producer loss."
    );
    let tokens = quote! {
        #![doc = #doc]

        /// This node's consumer-slot link_id.
        pub const LINK_ID: &str = #link_id;
        /// Core-node segment of the mock's wire identity.
        pub const MOCK_CORE_NODE: &str = #MOCK_CORE_NODE;
        /// Default instance segment of the mock's wire identity.
        pub const MOCK_INSTANCE_ID: &str = #default_instance;

        /// The default mock's wire identity, as the harness seeds it.
        pub fn producer_ref() -> peppylib::messaging::ProducerRef {
            producer_ref_for(MOCK_INSTANCE_ID)
        }

        /// [`producer_ref`] under an explicit instance id (multi-instance slots).
        pub fn producer_ref_for(instance_id: &str) -> peppylib::messaging::ProducerRef {
            peppylib::messaging::ProducerRef::new(MOCK_CORE_NODE, instance_id)
        }

        /// One mocked producer instance for this slot. Fields are the typed
        /// per-interface surfaces; construct with [`Mock::start`].
        pub struct Mock {
            _session: peppylib::MessengerHandle,
            #( #fields ),*
        }

        impl Mock {
            /// Connects a dedicated session and declares every interface of
            /// this dependency now (publishers, queryables, action engines),
            /// under the default [`MOCK_INSTANCE_ID`].
            pub async fn start(
                router: &peppylib::testing::EphemeralRouter,
            ) -> crate::Result<Self> {
                Self::start_as(router, MOCK_INSTANCE_ID).await
            }

            /// [`Mock::start`] under an explicit instance id, for slots bound
            /// to several mock instances.
            pub async fn start_as(
                router: &peppylib::testing::EphemeralRouter,
                instance_id: &str,
            ) -> crate::Result<Self> {
                let session = router.connect().await?;
                #( #constructs )*
                Ok(Self {
                    _session: session,
                    #( #field_names ),*
                })
            }

            /// Simulates this producer dying mid-flight, deterministically:
            /// live action goals are disarmed (their eventual drop emits no
            /// clean close), then every declaration and the session drop,
            /// releasing the liveliness tokens consumers latch on.
            pub fn stop(self) {
                #( #action_stops )*
            }
        }

        #( #modules )*
    };
    Ok(render_tokens(tokens))
}

fn render_dep_topic(
    generator: &mut RustGenerator,
    _link_id: &str,
    _producer_name: &str,
    spec: &DepTopicSpec,
    target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let topic_name = spec.name.as_str();
    let production = production_module("consumed_topics", &[&spec.module_link, topic_name]);
    let schema_key =
        crate::generator::naming::consumed_topic_schema_key(&spec.module_link, topic_name);
    let struct_prefix = format!(
        "{}{}",
        to_camel_case(&crate::generator::naming::sanitize_component(&spec.module_link)),
        to_camel_case(&crate::generator::naming::sanitize_component(topic_name)),
    );
    let serialize_ident = Ident::new("serialize_message", Span::call_site());
    let serializer = build_struct_serializer(
        generator,
        &serialize_ident,
        &schema_key,
        &struct_prefix,
        &spec.format,
        &quote!(Message),
        &format!("mock publish {topic_name}"),
    )?;

    let doc = format!(
        "Typed mock publisher for the consumed topic `{topic_name}`: publishes over the \
         real wire as this mock's identity; the first publish waits for the node's \
         subscription to match (no sleeps), and no subscriber within the readiness \
         timeout is a loud error."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            pub use #production::Message;
            #[allow(unused_imports)]
            use #production::*;

            pub struct Publisher {
                core: peppylib::testing::TestTopicPublisher,
            }

            impl Publisher {
                pub(super) async fn declare(
                    session: &peppylib::MessengerHandle,
                    instance_id: &str,
                ) -> crate::Result<Self> {
                    let core = peppylib::testing::TestTopicPublisher::declare(
                        session,
                        super::MOCK_CORE_NODE,
                        instance_id,
                        #target,
                        None,
                        #topic_name,
                        peppylib::config::QoSProfile::Standard,
                    )
                    .await?;
                    Ok(Self { core })
                }

                /// Publishes one typed message; lazily waits for the node's
                /// subscription before the first delivery.
                pub async fn publish(&self, message: &Message) -> crate::Result<()> {
                    self.core.publish(serialize_message(message)?).await
                }

                /// Waits until the node's subscription for this topic is
                /// visible; returns whether it matched within `timeout`.
                pub async fn wait_for_subscriber(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<bool> {
                    self.core.wait_for_subscriber(timeout).await
                }
            }

            #serializer
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Publisher),
        construct: quote! {
            let #module_ident = #module_ident::Publisher::declare(&session, instance_id).await?;
        },
        module_ident,
    })
}

fn render_dep_service(
    generator: &mut RustGenerator,
    _link_id: &str,
    producer_name: &str,
    spec: &DepServiceSpec,
    target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let service_name = spec.name.as_str();
    let production = production_module("consumed_services", &[&spec.module_link, service_name]);
    let request_key = crate::generator::naming::consumed_service_request_schema_key(
        producer_name,
        service_name,
    );
    let response_key = crate::generator::naming::consumed_service_response_schema_key(
        producer_name,
        service_name,
    );
    let struct_prefix = identifiers::consumed_service_struct_prefix(service_name);

    let deserialize_ident = Ident::new("deserialize_request", Span::call_site());
    let serialize_ident = Ident::new("serialize_response", Span::call_site());

    let request_codec = match &spec.request {
        Some(format) => Some(build_struct_deserializer(
            generator,
            &deserialize_ident,
            &request_key,
            &struct_prefix,
            format,
            &quote!(Request),
            &struct_prefix,
            &format!("mock {service_name} request"),
        )?),
        None => None,
    };
    let response_codec = match &spec.response {
        Some(format) => Some(build_struct_serializer(
            generator,
            &serialize_ident,
            &response_key,
            &struct_prefix,
            format,
            &quote!(ResponseData),
            &format!("mock {service_name} response"),
        )?),
        None => None,
    };

    let request_reexport = spec
        .request
        .as_ref()
        .map(|_| quote!(pub use #production::Request;));
    let response_reexport = spec
        .response
        .as_ref()
        .map(|_| quote!(pub use #production::ResponseData;));

    let response_payload = if spec.response.is_some() {
        quote!(serialize_response(&response)?)
    } else {
        quote!(peppylib::Payload::new())
    };
    let response_param = if spec.response.is_some() {
        quote!(response: ResponseData)
    } else {
        quote!()
    };

    // Request-less services park callers just the same, but there is nothing
    // to decode: next_request yields only the responder, and the capture
    // buffer degrades to a call count.
    let (next_request_method, captured_method) = if spec.request.is_some() {
        (
            quote! {
                /// The next unscripted request, decoded, with the responder
                /// the test must use to answer it. Errors after `timeout`.
                pub async fn next_request(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<(Request, Responder)> {
                    let (context, responder) = self.core.next_request(timeout).await?;
                    let request = deserialize_request(
                        context.message().payload_bytes().as_ref(),
                    )?;
                    Ok((request, Responder { inner: responder }))
                }
            },
            quote! {
                /// Every request received so far (scripted and manual alike),
                /// decoded, in arrival order.
                pub fn captured(&self) -> crate::Result<Vec<Request>> {
                    self.core
                        .captured()
                        .iter()
                        .map(|captured| {
                            deserialize_request(captured.message.payload_bytes().as_ref())
                        })
                        .collect()
                }
            },
        )
    } else {
        (
            quote! {
                /// Parks until the node's next unscripted call, returning the
                /// responder the test must use to answer it. Errors after
                /// `timeout`.
                pub async fn next_request(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<Responder> {
                    let (_context, responder) = self.core.next_request(timeout).await?;
                    Ok(Responder { inner: responder })
                }
            },
            quote! {
                /// How many calls the node has made so far.
                pub fn captured_count(&self) -> usize {
                    self.core.captured().len()
                }
            },
        )
    };

    let enqueue_doc = "Enqueues one response to be served automatically to the next \
                       inbound request (FIFO across repeated calls); only unscripted \
                       requests park for `next_request`.";
    let doc = format!(
        "Typed mock server for the consumed service `{service_name}`: a background pump \
         captures every request; scripted responses serve automatically, unscripted \
         requests park for `next_request`."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            #request_reexport
            #response_reexport
            #[allow(unused_imports)]
            use #production::*;

            pub struct Service {
                core: peppylib::testing::MockServiceCore,
            }

            /// Answers exactly one parked request; consumed by use.
            pub struct Responder {
                inner: peppylib::messaging::ServiceResponder,
            }

            impl Service {
                pub(super) async fn listen(
                    session: &peppylib::MessengerHandle,
                    instance_id: &str,
                ) -> crate::Result<Self> {
                    let core = peppylib::testing::MockServiceCore::listen(
                        session,
                        super::MOCK_CORE_NODE,
                        instance_id,
                        #target,
                        #service_name,
                    )
                    .await?;
                    Ok(Self { core })
                }

                #next_request_method

                #[doc = #enqueue_doc]
                pub fn enqueue_response(&self, #response_param) -> crate::Result<()> {
                    self.core.enqueue_response(#response_payload);
                    Ok(())
                }

                #captured_method
            }

            impl Responder {
                /// Sends the typed response for the parked request.
                pub async fn respond(self, #response_param) -> crate::Result<()> {
                    self.inner.respond(#response_payload).await
                }

                /// Fails the parked request with a handler-error reason.
                pub async fn respond_error(self, reason: impl Into<String>) -> crate::Result<()> {
                    self.inner.respond_error(reason.into()).await
                }
            }

            #request_codec
            #response_codec
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Service),
        construct: quote! {
            let #module_ident = #module_ident::Service::listen(&session, instance_id).await?;
        },
        module_ident,
    })
}

fn render_dep_action(
    generator: &mut RustGenerator,
    _link_id: &str,
    producer_name: &str,
    spec: &DepActionSpec,
    target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let action_name = spec.name.as_str();
    let production = production_module("consumed_actions", &[&spec.module_link, action_name]);
    let keys =
        crate::generator::naming::consumed_action_schema_keys(producer_name, action_name);
    let action_struct_name = format!(
        "{}Action",
        identifiers::consumed_action_type_prefix(producer_name, action_name)
    );
    let has_feedback = spec.messages.feedback.is_some();

    let mut codecs: Vec<TokenStream> = Vec::new();
    let mut reexports: Vec<TokenStream> = Vec::new();
    if spec.messages.goal_request.is_some() {
        reexports.push(quote!(pub use #production::GoalRequest;));
    }
    if spec.messages.goal_response.is_some() {
        reexports.push(quote!(pub use #production::GoalResponseData;));
    }
    if spec.messages.feedback.is_some() {
        reexports.push(quote!(pub use #production::FeedbackMessage;));
    }
    if spec.messages.result_response.is_some() {
        reexports.push(quote!(pub use #production::ResultResponseData;));
    }

    // Goal request: the node serializes, the mock decodes (reusing the
    // production `GoalRequest` struct, whose nested companions carry the
    // `{Action}Goal` prefix).
    let request_field = if let Some(format) = &spec.messages.goal_request {
        let fn_ident = Ident::new("deserialize_goal_request", Span::call_site());
        codecs.push(build_struct_deserializer(
            generator,
            &fn_ident,
            &keys.goal_request,
            &format!("{action_struct_name}GoalMessage"),
            format,
            &quote!(GoalRequest),
            &format!("{action_struct_name}Goal"),
            &format!("mock {action_name} goal request"),
        )?);
        Some(quote! {
            /// The decoded goal request.
            pub request: GoalRequest,
        })
    } else {
        None
    };
    let decode_request = if spec.messages.goal_request.is_some() {
        quote! {
            let request = deserialize_goal_request(pending.request_bytes())?;
        }
    } else {
        quote!()
    };
    let request_init = if spec.messages.goal_request.is_some() {
        quote!(request,)
    } else {
        quote!()
    };

    let (accept_param, accept_payload, reject_param, reject_payload) =
        if let Some(format) = &spec.messages.goal_response {
            let fn_ident = Ident::new("serialize_goal_response", Span::call_site());
            codecs.push(build_struct_serializer(
                generator,
                &fn_ident,
                &keys.goal_response,
                &format!("{action_struct_name}GoalResponse"),
                format,
                &quote!(GoalResponseData),
                &format!("mock {action_name} goal response"),
            )?);
            (
                quote!(response: GoalResponseData),
                quote!(serialize_goal_response(&response)?),
                quote!(response: Option<GoalResponseData>),
                quote! {
                    match response {
                        Some(response) => serialize_goal_response(&response)?,
                        None => peppylib::Payload::new(),
                    }
                },
            )
        } else {
            (
                quote!(),
                quote!(peppylib::Payload::new()),
                quote!(),
                quote!(peppylib::Payload::new()),
            )
        };

    let publish_feedback_method = if let Some(format) = &spec.messages.feedback {
        let fn_ident = Ident::new("serialize_feedback", Span::call_site());
        codecs.push(build_struct_serializer(
            generator,
            &fn_ident,
            &keys.feedback,
            &format!("{action_struct_name}FeedbackMessage"),
            format,
            &quote!(FeedbackMessage),
            &format!("mock {action_name} feedback"),
        )?);
        quote! {
            /// Publishes one typed feedback message for this goal.
            pub async fn publish_feedback(
                &self,
                feedback: &FeedbackMessage,
            ) -> crate::Result<()> {
                let payload = serialize_feedback(feedback)?;
                let payload = peppylib::messaging::NonEmptyPayload::try_new(payload)
                    .map_err(|_| peppylib::PeppyError::Io(std::io::Error::other(
                        "feedback serialized to zero bytes; empty is reserved for \
                         end-of-stream",
                    )))?;
                self.context.publish_feedback(payload).await?;
                Ok(())
            }
        }
    } else {
        quote!()
    };

    let (complete_param, complete_payload) = if let Some(format) = &spec.messages.result_response {
        let fn_ident = Ident::new("serialize_result", Span::call_site());
        codecs.push(build_struct_serializer(
            generator,
            &fn_ident,
            &keys.result_response,
            &format!("{action_struct_name}ResultResponse"),
            format,
            &quote!(ResultResponseData),
            &format!("mock {action_name} result"),
        )?);
        (quote!(result: &ResultResponseData), quote!(serialize_result(result)?))
    } else {
        (quote!(), quote!(peppylib::Payload::new()))
    };

    let doc = format!(
        "Typed mock action server for the consumed action `{action_name}`, on the real \
         `ConcurrentAction` engine: identical goal lifecycle to a production producer, \
         plus deterministic producer-loss via the owning mock's `stop()`."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            #( #reexports )*
            #[allow(unused_imports)]
            use #production::*;

            pub struct Action {
                core: peppylib::testing::MockActionServerCore,
            }

            impl Action {
                pub(super) async fn expose(
                    session: &peppylib::MessengerHandle,
                    instance_id: &str,
                ) -> crate::Result<Self> {
                    let core = peppylib::testing::MockActionServerCore::expose(
                        session,
                        super::MOCK_CORE_NODE,
                        instance_id,
                        #target,
                        #action_name,
                        #has_feedback,
                    )
                    .await?;
                    Ok(Self { core })
                }

                /// Parks until the node sends a goal, bounded by `timeout`;
                /// the returned goal awaits the test's admission decision.
                pub async fn next_goal(
                    &mut self,
                    timeout: std::time::Duration,
                ) -> crate::Result<PendingGoal> {
                    let pending = self.core.next_goal(timeout).await?;
                    #decode_request
                    Ok(PendingGoal { pending, #request_init })
                }

                pub(super) fn stop(self) {
                    self.core.stop();
                }
            }

            /// A goal received by the mock, awaiting accept/reject.
            pub struct PendingGoal {
                pending: peppylib::testing::MockPendingGoal,
                #request_field
            }

            impl PendingGoal {
                /// The client-generated correlation id for this goal.
                pub fn goal_id(&self) -> &str {
                    self.pending.goal_id()
                }

                /// Accepts the goal; the returned handle drives feedback and
                /// completion.
                pub async fn accept(self, #accept_param) -> crate::Result<ActiveGoal> {
                    let context = self.pending.accept(#accept_payload).await?;
                    Ok(ActiveGoal { context })
                }

                /// Rejects the goal with an optional human-readable reason.
                pub async fn reject(
                    self,
                    reason: Option<&str>,
                    #reject_param
                ) -> crate::Result<()> {
                    self.pending.reject(reason, #reject_payload).await
                }
            }

            /// An accepted goal: drives feedback and terminal completion.
            pub struct ActiveGoal {
                context: std::sync::Arc<peppylib::messaging::GoalContext>,
            }

            impl ActiveGoal {
                #publish_feedback_method

                /// Resolves when the node requests cancellation of this goal.
                pub async fn cancel_signal(&self) {
                    self.context.cancel_signal().await
                }

                /// Whether cancellation has been requested for this goal.
                pub fn is_cancelled(&self) -> bool {
                    self.context.is_cancelled()
                }

                /// Completes the goal successfully.
                pub async fn complete(&self, #complete_param) -> crate::Result<()> {
                    self.context.complete(#complete_payload).await?;
                    Ok(())
                }

                /// Completes the goal as cancelled.
                pub async fn complete_cancelled(&self, #complete_param) -> crate::Result<()> {
                    self.context.complete_cancelled(#complete_payload).await?;
                    Ok(())
                }
            }

            #( #codecs )*
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Action),
        construct: quote! {
            let #module_ident = #module_ident::Action::expose(&session, instance_id).await?;
        },
        module_ident,
    })
}

// ---------------------------------------------------------------------------
// pairings
// ---------------------------------------------------------------------------

fn render_pairing_link(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &PairingLinkSpec,
) -> Result<String> {
    let mut seen = HashSet::new();
    let mut owners = std::collections::HashMap::new();
    let mut members: Vec<MemberModule> = Vec::new();
    let pairing_target = target_expr(&TargetSpec::Pairing {
        name: spec.pairing_name.clone(),
        tag: spec.pairing_tag.clone(),
    });

    // Topics the node consumes: the mock publishes as the paired peer.
    for topic in &spec.node_consumes {
        let module_ident = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_pair_publisher(
            generator,
            link_id,
            topic,
            &pairing_target,
            module_ident,
        )?);
    }
    // Topics the node emits: the mock subscribes, triple-pinned to the node.
    for topic in &spec.node_emits {
        let module_ident = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_pair_subscription(
            generator,
            link_id,
            topic,
            &pairing_target,
            module_ident,
        )?);
    }

    let default_instance = mock_instance_id(link_id);
    let pairing_name = spec.pairing_name.as_str();
    let pairing_tag = spec.pairing_tag.as_str();
    let modules: Vec<&TokenStream> = members.iter().map(|member| &member.module).collect();
    let fields: Vec<TokenStream> = members
        .iter()
        .map(|member| {
            let name = &member.module_ident;
            let ty = &member.field_ty;
            quote!(pub #name: #ty)
        })
        .collect();
    let constructs: Vec<&TokenStream> = members.iter().map(|member| &member.construct).collect();
    let field_names: Vec<&Ident> = members.iter().map(|member| &member.module_ident).collect();

    let doc = format!(
        "Mock peer for the `{link_id}` pairing slot: publishes the topics the node \
         consumes and holds triple-pinned subscriptions to the topics the node emits, \
         under the pin identity the harness seeds (`peer_info()`)."
    );
    let tokens = quote! {
        #![doc = #doc]

        /// This node's own pairing-slot link_id.
        pub const LINK_ID: &str = #link_id;
        pub const PAIRING_NAME: &str = #pairing_name;
        pub const PAIRING_TAG: &str = #pairing_tag;
        /// The mock peer's own (complementary) slot id: what the node's
        /// `paired()` reports as `peer_link_id` under the harness.
        pub const PEER_LINK_ID: &str = #MOCK_PEER_LINK_ID;
        /// Core-node segment of the mock's wire identity.
        pub const MOCK_CORE_NODE: &str = #MOCK_CORE_NODE;
        /// Instance segment of the mock's wire identity.
        pub const MOCK_INSTANCE_ID: &str = #default_instance;

        /// The mock peer's wire identity.
        pub fn producer_ref() -> peppylib::messaging::ProducerRef {
            peppylib::messaging::ProducerRef::new(MOCK_CORE_NODE, MOCK_INSTANCE_ID)
        }

        /// The full pin identity the harness seeds for this slot: what the
        /// node's `paired()` / `wait_paired()` resolve to.
        pub fn peer_info() -> peppylib::messaging::PeerInfo {
            peppylib::messaging::PeerInfo {
                producer: producer_ref(),
                peer_link_id: PEER_LINK_ID.to_string(),
            }
        }

        /// The mock peer for this slot; construct with [`Mock::start`].
        pub struct Mock {
            _session: peppylib::MessengerHandle,
            #( #fields ),*
        }

        impl Mock {
            /// Connects a dedicated session, declares the peer publishers and
            /// opens the pinned subscriptions to the node's emissions.
            /// `node_instance_id` is the node-under-test's instance id (the
            /// harness passes its own).
            pub async fn start(
                router: &peppylib::testing::EphemeralRouter,
                node_instance_id: &str,
            ) -> crate::Result<Self> {
                let session = router.connect().await?;
                #( #constructs )*
                Ok(Self {
                    _session: session,
                    #( #field_names ),*
                })
            }

            /// Simulates the peer disappearing: every declaration and the
            /// session drop.
            pub fn stop(self) {}
        }

        #( #modules )*
    };
    Ok(render_tokens(tokens))
}

fn render_pair_publisher(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let topic_name = spec.name.as_str();
    let production = production_module("paired_topics", &[link_id, topic_name]);
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let schema_prefix = to_camel_case(&schema_key);
    let qos = qos_tokens(&spec.qos);
    let serialize_ident = Ident::new("serialize_message", Span::call_site());
    let serializer = build_struct_serializer(
        generator,
        &serialize_ident,
        &schema_key,
        &schema_prefix,
        &spec.format,
        &quote!(Message),
        &format!("mock peer publish {topic_name}"),
    )?;

    let doc = format!(
        "Typed peer publisher for `{topic_name}` (the node consumes this direction): \
         publishes under the mock peer's identity and slot id, so the node's \
         triple-pinned subscription receives it."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            pub use #production::Message;
            #[allow(unused_imports)]
            use #production::*;

            pub struct Publisher {
                core: peppylib::testing::TestTopicPublisher,
            }

            impl Publisher {
                pub(super) async fn declare(
                    session: &peppylib::MessengerHandle,
                ) -> crate::Result<Self> {
                    let core = peppylib::testing::TestTopicPublisher::declare(
                        session,
                        super::MOCK_CORE_NODE,
                        super::MOCK_INSTANCE_ID,
                        #pairing_target,
                        Some(super::PEER_LINK_ID),
                        #topic_name,
                        #qos,
                    )
                    .await?;
                    Ok(Self { core })
                }

                /// Publishes one typed message; lazily waits for the node's
                /// pinned subscription before the first delivery.
                pub async fn publish(&self, message: &Message) -> crate::Result<()> {
                    self.core.publish(serialize_message(message)?).await
                }

                /// Waits until the node's pinned subscription is visible;
                /// returns whether it matched within `timeout`.
                pub async fn wait_for_subscriber(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<bool> {
                    self.core.wait_for_subscriber(timeout).await
                }
            }

            #serializer
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Publisher),
        construct: quote! {
            let #module_ident = #module_ident::Publisher::declare(&session).await?;
        },
        module_ident,
    })
}

fn render_pair_subscription(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let topic_name = spec.name.as_str();
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let schema_prefix = to_camel_case(&schema_key);
    let qos = qos_tokens(&spec.qos);
    let deserialize_ident = Ident::new("deserialize_message", Span::call_site());
    // The node-emitted direction has no production consumer struct (the
    // production module only serializes), so the veneer defines its own.
    let message_and_deserializer = build_local_message_with_deserializer(
        generator,
        &schema_key,
        &schema_prefix,
        &spec.format,
        &deserialize_ident,
        &format!("mock peer receive {topic_name}"),
    )?;

    let doc = format!(
        "Typed subscription to `{topic_name}` (the node emits this direction): pinned \
         to the node's identity and slot id exactly as a real paired peer would be."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            /// A held subscription to the node's emissions on this slot.
            pub struct Subscription {
                inner: peppylib::messaging::Subscription,
            }

            impl Subscription {
                pub(super) async fn open(
                    session: &peppylib::MessengerHandle,
                    node_instance_id: &str,
                ) -> crate::Result<Self> {
                    // The exact wire shape of a paired peer's subscription:
                    // node identity, pairing target, and the node's own slot
                    // link_id all pinned. No pin-following — the mock's peer
                    // (the node under test) is known from construction.
                    let node = peppylib::messaging::ProducerRef::new(
                        "standalone-core",
                        node_instance_id,
                    );
                    let inner = peppylib::testing::subscribe_peer_pinned(
                        session,
                        super::MOCK_CORE_NODE,
                        super::MOCK_INSTANCE_ID,
                        #pairing_target,
                        &node,
                        super::LINK_ID,
                        #topic_name,
                        #qos,
                    )
                    .await?;
                    Ok(Self { inner })
                }

                /// Awaits the node's next message on this topic; `Ok(None)`
                /// once the mock's session closes.
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
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Subscription),
        construct: quote! {
            let #module_ident =
                #module_ident::Subscription::open(&session, node_instance_id).await?;
        },
        module_ident,
    })
}

// ---------------------------------------------------------------------------
// observed
// ---------------------------------------------------------------------------

fn render_observed_link(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &ObservedLinkSpec,
) -> Result<String> {
    let mut seen = HashSet::new();
    let mut owners = std::collections::HashMap::new();
    let mut members: Vec<MemberModule> = Vec::new();
    let pairing_target = target_expr(&TargetSpec::Pairing {
        name: spec.pairing_name.clone(),
        tag: spec.pairing_tag.clone(),
    });

    for topic in &spec.topics {
        let module_ident = claim_member_name(link_id, &topic.name, &mut seen, &mut owners)?;
        members.push(render_observed_publisher(
            generator,
            link_id,
            topic,
            &pairing_target,
            module_ident,
        )?);
    }

    let default_instance = mock_instance_id(link_id);
    let pairing_name = spec.pairing_name.as_str();
    let pairing_tag = spec.pairing_tag.as_str();
    let modules: Vec<&TokenStream> = members.iter().map(|member| &member.module).collect();
    let fields: Vec<TokenStream> = members
        .iter()
        .map(|member| {
            let name = &member.module_ident;
            let ty = &member.field_ty;
            quote!(pub #name: #ty)
        })
        .collect();
    let constructs: Vec<&TokenStream> = members.iter().map(|member| &member.construct).collect();
    let field_names: Vec<&Ident> = members.iter().map(|member| &member.module_ident).collect();

    let doc = format!(
        "Mock observed source for the `{link_id}` observer slot: a publish-only pairing \
         member under the source identity the harness seeds (`source()`); multi-member \
         slots start several instances with distinct instance ids."
    );
    let tokens = quote! {
        #![doc = #doc]

        /// This node's own observer-slot link_id.
        pub const LINK_ID: &str = #link_id;
        pub const PAIRING_NAME: &str = #pairing_name;
        pub const PAIRING_TAG: &str = #pairing_tag;
        /// The producer-side link_id mock sources publish under.
        pub const SOURCE_LINK_ID: &str = #MOCK_SOURCE_LINK_ID;
        /// Core-node segment of the mock's wire identity.
        pub const MOCK_CORE_NODE: &str = #MOCK_CORE_NODE;
        /// Default instance segment of the mock's wire identity.
        pub const MOCK_INSTANCE_ID: &str = #default_instance;

        /// The default mock source, as the harness seeds it.
        pub fn source() -> peppylib::messaging::ObservedSource {
            source_for(MOCK_INSTANCE_ID)
        }

        /// [`source`] under an explicit instance id (multi-member slots).
        pub fn source_for(instance_id: &str) -> peppylib::messaging::ObservedSource {
            peppylib::messaging::ObservedSource {
                producer: peppylib::messaging::ProducerRef::new(MOCK_CORE_NODE, instance_id),
                source_link_id: SOURCE_LINK_ID.to_string(),
            }
        }

        /// One mock source instance for this slot; construct with
        /// [`Mock::start`] / [`Mock::start_as`].
        pub struct Mock {
            _session: peppylib::MessengerHandle,
            #( #fields ),*
        }

        impl Mock {
            /// Connects a dedicated session and declares this source's
            /// publishers under the default [`MOCK_INSTANCE_ID`].
            pub async fn start(
                router: &peppylib::testing::EphemeralRouter,
            ) -> crate::Result<Self> {
                Self::start_as(router, MOCK_INSTANCE_ID).await
            }

            /// [`Mock::start`] under an explicit instance id, for observing
            /// several mock sources at once.
            pub async fn start_as(
                router: &peppylib::testing::EphemeralRouter,
                instance_id: &str,
            ) -> crate::Result<Self> {
                let session = router.connect().await?;
                #( #constructs )*
                Ok(Self {
                    _session: session,
                    #( #field_names ),*
                })
            }

            /// Simulates this source disappearing: every declaration and the
            /// session drop. (Standalone observation liveness never changes,
            /// so the node keeps observing; messages simply stop.)
            pub fn stop(self) {}
        }

        #( #modules )*
    };
    Ok(render_tokens(tokens))
}

fn render_observed_publisher(
    generator: &mut RustGenerator,
    link_id: &str,
    spec: &PairTopicSpec,
    pairing_target: &TokenStream,
    module_ident: Ident,
) -> Result<MemberModule> {
    let topic_name = spec.name.as_str();
    let production = production_module("paired_topics", &[link_id, topic_name]);
    let schema_key = crate::generator::naming::peer_schema_key(link_id, topic_name);
    let schema_prefix = to_camel_case(&schema_key);
    let qos = qos_tokens(&spec.qos);
    let serialize_ident = Ident::new("serialize_message", Span::call_site());
    let serializer = build_struct_serializer(
        generator,
        &serialize_ident,
        &schema_key,
        &schema_prefix,
        &spec.format,
        &quote!(Message),
        &format!("mock observed publish {topic_name}"),
    )?;

    let doc = format!(
        "Typed source publisher for the observed topic `{topic_name}`: publishes under \
         the mock source's identity and source link_id, so the node's \
         generation-checked observation subscription receives it."
    );
    let module = quote! {
        #[doc = #doc]
        pub mod #module_ident {
            pub use #production::Message;
            #[allow(unused_imports)]
            use #production::*;

            pub struct Publisher {
                core: peppylib::testing::TestTopicPublisher,
            }

            impl Publisher {
                pub(super) async fn declare(
                    session: &peppylib::MessengerHandle,
                    instance_id: &str,
                ) -> crate::Result<Self> {
                    let core = peppylib::testing::TestTopicPublisher::declare(
                        session,
                        super::MOCK_CORE_NODE,
                        instance_id,
                        #pairing_target,
                        Some(super::SOURCE_LINK_ID),
                        #topic_name,
                        #qos,
                    )
                    .await?;
                    Ok(Self { core })
                }

                /// Publishes one typed message; lazily waits for the node's
                /// observation subscription before the first delivery.
                pub async fn publish(&self, message: &Message) -> crate::Result<()> {
                    self.core.publish(serialize_message(message)?).await
                }

                /// Waits until the node's observation subscription is
                /// visible; returns whether it matched within `timeout`.
                pub async fn wait_for_subscriber(
                    &self,
                    timeout: std::time::Duration,
                ) -> crate::Result<bool> {
                    self.core.wait_for_subscriber(timeout).await
                }
            }

            #serializer
        }
    };
    Ok(MemberModule {
        module,
        field_ty: quote!(#module_ident::Publisher),
        construct: quote! {
            let #module_ident = #module_ident::Publisher::declare(&session, instance_id).await?;
        },
        module_ident,
    })
}
