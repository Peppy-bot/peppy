use super::deserialization::build_deserialize_fn;
use super::serialization::MessageEncodingSpec;
use super::serialization::build_serialize_payload;
use super::services::{
    build_result_expr_from_values, build_return_type_from_params, deserialize_fields_from_format,
};
use super::topics::{EmitMethodSpec, build_emit_method};
use crate::error::Result;
use crate::generator::naming::cancel_reason_parameter;
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

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            let inner = peppylib::messaging::ConcurrentAction::expose(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #target_expr,
                ACTION_NAME,
                #has_feedback,
            )
            .await?;

            Ok(Self { inner })
        }

        /// Sets the sender grace of the action: how long it keeps the goals
        /// of a sender that is gone before they receive their cancel signal,
        /// with the cause `CancelCause::SenderGone`. Zero by default. It
        /// applies to each sender found gone after the call.
        pub fn set_sender_grace(&self, grace: std::time::Duration) {
            self.inner.set_sender_grace(grace);
        }
    }
}

/// The decision a goal handler returns. Admission travels in the framework
/// goal-ack envelope, so it stays independent of the declared `GoalResponse`
/// fields: an accept carries the declared response when one exists, and a
/// refusal carries a failure of a kind the member declares, and no response.
pub fn build_goal_decision_enum(has_response: bool) -> TokenStream {
    let (accept_variant, accept_fn) = if has_response {
        (
            quote!(Accept(GoalResponse),),
            quote! {
                /// Accepts the goal and replies with the declared response.
                pub fn accept(response: GoalResponse) -> Self {
                    Self::Accept(response)
                }
            },
        )
    } else {
        (
            quote!(Accept,),
            quote! {
                /// Accepts the goal.
                pub fn accept() -> Self {
                    Self::Accept
                }
            },
        )
    };
    quote! {
        #[allow(dead_code)]
        pub enum GoalDecision {
            #accept_variant
            Refuse(Failure),
        }

        #[allow(dead_code)]
        impl GoalDecision {
            #accept_fn

            /// Refuses the goal with a failure kind that this member
            /// declares and a message for a person. A refusal carries no
            /// goal response.
            pub fn refuse(kind: FailureKind, message: impl Into<String>) -> Self {
                Self::Refuse(Failure::new(kind, message))
            }
        }
    }
}

/// `handle_goal_next_request`: returns the next *accepted* goal as a
/// `GoalContext`. The user decider runs on each incoming goal; refused goals
/// are answered and skipped transparently (the method keeps polling), so a
/// returned `Ok(None)` means the goal stream has closed (the node is shutting
/// down) and the caller should stop its accept loop. A goal whose request
/// bytes do not decode into the declared `GoalRequestData` (a field missing,
/// a fixed-length array of the wrong length) is refused the same way, with
/// the kind `rejected` and the decode error as the message, before the
/// decider ever sees it: the client sent something this server's shape does
/// not admit, which is the client's problem to learn about, not a reason to
/// stop serving. A decider that returns `Err` refuses the goal with the kind
/// `attempt_failed` and the error as the message, and the method keeps
/// polling, as the Python handler that raises does. Transport errors
/// propagate as `Err`.
///
/// `response_serialization`, when present, serializes a local `response`
/// (`GoalResponse`) into a `peppylib::Payload`.
pub fn build_handle_goal_next_request(
    has_request_data: bool,
    response_serialization: Option<TokenStream>,
) -> TokenStream {
    let decode_and_build_request = if has_request_data {
        quote! {
            let request_data = match deserialize_goal_request(pending.request_bytes()) {
                Ok(request_data) => request_data,
                Err(error) => {
                    // The request does not fit this server's declared shape:
                    // refuse the goal with the decode error and keep polling
                    // for the next goal.
                    let refusal = Failure::new(
                        FailureKind::Rejected,
                        format!("goal request does not decode: {error}"),
                    );
                    pending.refuse(refusal.to_wire()).await?;
                    continue;
                }
            };
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

    let accept_arm = match response_serialization {
        Some(serialize) => quote! {
            GoalDecision::Accept(response) => {
                let response_payload = #serialize;
                let inner = pending.accept(response_payload).await?;
                return Ok(Some(GoalContext { inner, request }));
            }
        },
        None => quote! {
            GoalDecision::Accept => {
                let inner = pending.accept(peppylib::Payload::new()).await?;
                return Ok(Some(GoalContext { inner, request }));
            }
        },
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
                let decision = match decider(&request) {
                    Ok(decision) => decision,
                    Err(error) => {
                        // The decider failed: the goal gets a refusal with
                        // the error, and the loop polls for the next goal.
                        let refusal = Failure::new(
                            FailureKind::AttemptFailed,
                            format!("the goal decider failed: {error}"),
                        );
                        pending.refuse(refusal.to_wire()).await?;
                        continue;
                    }
                };
                match decision {
                    #accept_arm
                    // Refused: answer the client and keep polling for the
                    // next goal. A refusal never ends the accept loop.
                    GoalDecision::Refuse(failure) => {
                        pending.refuse(failure.to_wire()).await?;
                    }
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

/// `GoalContext` methods present for every action: the request, its sender
/// and goal_id, the cancel signal and its cause, and `fail`.
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

        /// The process that sent this goal. The action cancels the goal
        /// when this process is gone and its sender grace passed.
        pub fn sender(&self) -> &peppylib::messaging::ClientPresence {
            self.inner.sender()
        }

        /// Resolves when this goal receives its cancel signal: a cancel
        /// request of its caller, or the end of its sender (see
        /// `cancel_cause`). React by calling `complete_cancelled`.
        pub async fn cancel_signal(&self) {
            self.inner.cancel_signal().await
        }

        /// Whether this goal received its cancel signal.
        pub fn is_cancelled(&self) -> bool {
            self.inner.is_cancelled()
        }

        /// Why this goal received its cancel signal; `None` before it did.
        /// `CancelCause::reason` gives a reason for `complete_cancelled`.
        pub fn cancel_cause(&self) -> Option<peppylib::messaging::CancelCause> {
            self.inner.cancel_cause()
        }

        /// Ends the goal with the status failed: a failure kind that this
        /// member declares, a message for a person, and no result. The
        /// first of `complete`, `fail` and `complete_cancelled` ends the
        /// goal; later calls do nothing.
        pub async fn fail(
            &self,
            kind: FailureKind,
            message: impl Into<String>,
        ) -> crate::Result<()> {
            self.inner.fail(Failure::new(kind, message).to_wire()).await?;
            Ok(())
        }
    }
}

/// `publish_feedback(fields…)` on `GoalContext`, serializing the feedback
/// message and publishing it on this goal's per-goal stream.
pub fn build_goal_context_publish_feedback(
    params: &[FunctionParam],
    encoding: &MessageEncodingSpec,
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
    })
}

/// Which terminal result method of `GoalContext` to build.
#[derive(Clone, Copy)]
pub enum ResultDelivery {
    /// `complete(fields…)`: the goal is a success.
    Completed,
    /// `complete_cancelled(reason, fields…)`: the goal ends cancelled, for
    /// `reason`.
    Cancelled,
}

/// `complete(fields…)` / `complete_cancelled(reason, fields…)` on
/// `GoalContext`, serializing the result response and delivering it. Handles
/// an action that declares no result (or an empty one) by sending an empty
/// payload.
pub fn build_goal_context_complete(
    delivery: ResultDelivery,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    let label_literal = Literal::string(label);
    let error_context = quote!(format!("{} {}", #label_literal, ACTION_NAME));

    let reason = Ident::new(
        &cancel_reason_parameter(|name| params.iter().any(|param| param.ident == name)),
        Span::call_site(),
    );
    let mut method_params: Vec<TokenStream> = Vec::new();
    if let ResultDelivery::Cancelled = delivery {
        method_params.push(quote!(#reason: impl Into<String>));
    }
    method_params.extend(params.iter().map(|param| {
        let ident = &param.ident;
        let ty = &param.ty;
        quote!(#ident: #ty)
    }));

    let serialize = match encoding {
        Some(spec) => {
            build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context)
        }
        None => quote!(peppylib::Payload::new()),
    };

    let (doc, method, delivery_call) = match delivery {
        ResultDelivery::Completed => (
            quote! {
                /// Ends the goal with the status completed and its result:
                /// the goal is a success. The first of `complete`, `fail`
                /// and `complete_cancelled` ends the goal; later calls do
                /// nothing.
            },
            quote!(complete),
            quote!(self.inner.complete(payload).await?;),
        ),
        ResultDelivery::Cancelled => (
            {
                let summary = Literal::string(&format!(
                    " Ends the goal with the status cancelled, for `{reason}`, with its result."
                ));
                quote! {
                    #[doc = #summary]
                    /// A worker calls it after the cancel signal
                    /// (`CancelCause::reason` gives a reason), and a member
                    /// that ends the goals of other clients calls it with a
                    /// reason of its own.
                }
            },
            quote!(complete_cancelled),
            quote!(self.inner.complete_cancelled(#reason, payload).await?;),
        ),
    };

    quote! {
        #doc
        #[allow(clippy::too_many_arguments)]
        pub async fn #method(&self, #(#method_params),*) -> crate::Result<()> {
            let payload = #serialize;
            #delivery_call
            Ok(())
        }
    }
}

/// Deserializer for the goal request payload into `GoalRequestData`.
/// `struct_prefix` is the prefix the request's nested message structs are
/// defined with, so the deserializer constructs the types the module
/// declares.
pub fn build_action_request_deserializer(
    deserializer_fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    request_format: &MessageFormat,
    params: &[FunctionParam],
    struct_prefix: &str,
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
        deserialize_fields_from_format(request_format, params, struct_prefix, &context_expr)?;

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
