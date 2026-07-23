use super::deserialization::build_deserialize_fn;
use super::serialization::MessageEncodingSpec;
use super::serialization::build_serialize_payload;
use super::services::{
    build_result_expr_from_values, build_return_type_from_params, deserialize_fields_from_format,
};
use super::topics::{EmitMethodSpec, build_emit_method};
use crate::error::Result;
use config::node::MessageFormat;
use encoding::FunctionParam;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

/// The generated `ActionHandle` wraps the peppylib concurrent-action engine,
/// which owns the goal/cancel/result services and routes cancel/result requests
/// to the right in-flight goal by `goal_id`.
pub fn build_action_handle_struct() -> TokenStream {
    quote! {
        pub struct ActionHandle {
            inner: peppylib::messaging::ConcurrentAction,
        }
    }
}

pub fn build_action_expose_method(
    has_feedback: bool,
    origin: Option<&crate::generator::types::ContractOrigin>,
) -> TokenStream {
    let target_expr = super::topics::sender_target_expression(origin);
    let has_feedback_lit = if has_feedback {
        quote!(true)
    } else {
        quote!(false)
    };

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            let inner = peppylib::messaging::ConcurrentAction::expose(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #target_expr,
                ACTION_NAME,
                #has_feedback_lit,
            )
            .await?;

            Ok(Self { inner })
        }
    }
}

/// The framework decision returned by a goal handler. Accept and reject both
/// carry the declared `GoalResponse` when one exists, keeping control flow
/// independent of the response payload's fields.
pub fn build_goal_decision_enum(has_response: bool) -> TokenStream {
    if has_response {
        quote! {
            #[allow(dead_code)]
            pub enum GoalDecision {
                Accept(GoalResponse),
                Reject(GoalResponse),
            }
        }
    } else {
        quote! {
            #[allow(dead_code)]
            pub enum GoalDecision {
                Accept,
                Reject,
            }
        }
    }
}

/// `handle_goal_next_request`: returns the next *accepted* goal as a
/// `GoalContext`. The user decider runs on each incoming goal; rejected goals
/// are answered and skipped transparently (the method keeps polling), so a
/// returned `Ok(None)` means the goal stream has closed (the node is shutting
/// down) and the caller should stop its accept loop. Errors propagate as `Err`.
///
/// `response_serialization`, when present, serializes a local `response`
/// (`GoalResponse`) into a `peppylib::Payload`.
pub fn build_handle_goal_next_request(
    has_request_data: bool,
    response_serialization: Option<TokenStream>,
) -> TokenStream {
    let decode_and_build_request = if has_request_data {
        quote! {
            let request_data = deserialize_goal_request(pending.request_bytes())?;
            let request = GoalRequest {
                instance_id: pending.instance_id().to_string(),
                core_node: pending.core_node().to_string(),
                data: request_data,
            };
        }
    } else {
        quote! {
            let request = GoalRequest {
                instance_id: pending.instance_id().to_string(),
                core_node: pending.core_node().to_string(),
            };
        }
    };

    let (accept_arm, reject_arm) = match response_serialization {
        Some(serialize) => {
            let serialize_reject = serialize.clone();
            (
                quote! {
                    GoalDecision::Accept(response) => {
                        let response_payload = #serialize;
                        let inner = pending.accept(response_payload).await?;
                        return Ok(Some(GoalContext { inner, request }));
                    }
                },
                quote! {
                    // Rejected: answer the client and keep polling for the
                    // next goal. A reject never ends the accept loop.
                    GoalDecision::Reject(response) => {
                        let response_payload = #serialize_reject;
                        pending.reject(response_payload).await?;
                    }
                },
            )
        }
        None => (
            quote! {
                GoalDecision::Accept => {
                    let inner = pending.accept(peppylib::Payload::new()).await?;
                    return Ok(Some(GoalContext { inner, request }));
                }
            },
            quote! {
                // Rejected: answer the client and keep polling for the next
                // goal. A reject never ends the accept loop.
                GoalDecision::Reject => {
                    pending.reject(peppylib::Payload::new()).await?;
                }
            },
        ),
    };

    quote! {
        pub async fn handle_goal_next_request<F>(
            &mut self,
            decider: F,
        ) -> crate::Result<Option<GoalContext>>
        where
            F: Fn(&GoalRequest) -> crate::Result<GoalDecision>,
        {
            loop {
                let Some(pending) = self.inner.recv_next_goal().await? else {
                    // The goal stream has closed (node shutting down).
                    return Ok(None);
                };
                #decode_and_build_request
                match decider(&request)? {
                    #accept_arm
                    #reject_arm
                }
            }
        }
    }
}

/// The per-goal `GoalContext` handed to user code. Owns the decoded request and
/// the peppylib context (per-goal feedback publisher, cancel signal, result
/// delivery).
pub fn build_goal_context_struct() -> TokenStream {
    quote! {
        pub struct GoalContext {
            inner: peppylib::messaging::GoalContext,
            request: GoalRequest,
        }
    }
}

/// `GoalContext` methods present for every action: request accessor, goal_id,
/// and the cancel signal.
pub fn build_goal_context_base_methods() -> TokenStream {
    quote! {
        /// The decoded goal request (caller identity and, if any, data).
        pub fn request(&self) -> &GoalRequest {
            &self.request
        }

        /// The client-generated correlation id for this goal.
        pub fn goal_id(&self) -> &str {
            self.inner.goal_id()
        }

        /// Resolves when a cancel request arrives for this goal. React by
        /// calling `complete_cancelled`.
        pub async fn cancel_signal(&self) {
            self.inner.cancel_signal().await
        }

        /// Whether a cancel has been requested for this goal.
        pub fn is_cancelled(&self) -> bool {
            self.inner.is_cancelled()
        }
    }
}

/// `publish_feedback(fields…)` on `GoalContext`, serializing the feedback
/// message and publishing it on this goal's per-goal stream.
pub fn build_goal_context_publish_feedback(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    let label_literal = Literal::string(label);
    let method_ident = Ident::new("publish_feedback", Span::call_site());

    build_emit_method(EmitMethodSpec {
        method_name: &method_ident,
        params,
        encoding,
        receiver: quote!(&self),
        publish_body: quote! {
            let payload = peppylib::messaging::NonEmptyPayload::try_new(payload)
                .map_err(|_| peppylib::PeppyError::Io(std::io::Error::other(
                    "publish_feedback produced an empty payload (codec serialized \
                     to zero bytes); empty is reserved for end-of-stream"
                )))?;
            self.inner.publish_feedback(payload).await?;
        },
        error_context: quote!(format!("{} {}", #label_literal, ACTION_NAME)),
        suppress_unused: Vec::new(),
    })
}

/// `complete(fields…)` / `complete_cancelled(fields…)` on `GoalContext`,
/// serializing the result response and delivering it. `method_name` names both
/// the generated method and the peppylib method it forwards to (they are always
/// the same). Handles an empty result response (no fields) by sending an empty
/// payload.
pub fn build_goal_context_complete(
    method_name: &str,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    let method_ident = Ident::new(method_name, Span::call_site());
    let inner_ident = Ident::new(method_name, Span::call_site());
    let label_literal = Literal::string(label);
    let error_context = quote!(format!("{} {}", #label_literal, ACTION_NAME));

    let method_param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();
    let signature = if method_param_tokens.is_empty() {
        quote!(&self)
    } else {
        quote!(&self, #(#method_param_tokens),*)
    };

    let serialize = match encoding {
        Some(spec) => {
            build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context)
        }
        None => quote!(peppylib::Payload::new()),
    };

    quote! {
        #[allow(clippy::too_many_arguments)]
        pub async fn #method_ident(#signature) -> crate::Result<()> {
            let payload = #serialize;
            self.inner.#inner_ident(payload).await?;
            Ok(())
        }
    }
}

/// Deserializer for the goal request payload into `GoalRequestData`.
pub fn build_action_request_deserializer(
    deserializer_fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    request_format: &MessageFormat,
    params: &[FunctionParam],
    label: &str,
    request_struct: Option<&Ident>,
) -> Result<TokenStream> {
    let reader_type = &request_spec.reader_type;
    let return_ty = build_return_type_from_params(params, request_struct);
    let context_expr = quote!(format!(
        "{} {}",
        stringify!(#deserializer_fn_name),
        ACTION_NAME
    ));

    let (field_statements, value_idents) =
        deserialize_fields_from_format(request_format, params, label, &context_expr)?;

    let request_expr = build_result_expr_from_values(params, &value_idents, request_struct);

    Ok(build_deserialize_fn(
        deserializer_fn_name,
        reader_type,
        &context_expr,
        &return_ty,
        &field_statements,
        &request_expr,
    ))
}
