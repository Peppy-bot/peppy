use super::deserialization::build_deserialize_fn;
use super::serialization::MessageEncodingSpec;
use super::services::{
    ServiceResponseSpec, build_response_payload_tokens, build_result_expr_from_values,
    build_return_type_from_params, deserialize_fields_from_format,
};
use crate::error::Result;
use config::encoding::FunctionParam;
use config::node::MessageFormat;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

pub fn build_action_handle_struct(
    has_goal: bool,
    has_feedback: bool,
    has_result: bool,
) -> TokenStream {
    let mut fields = Vec::new();

    if has_goal {
        fields.push(quote!(goal_service: peppylib::messaging::ServiceEndpoint));
        fields.push(quote!(cancel_service: peppylib::messaging::ServiceEndpoint));
    }

    if has_result {
        fields.push(quote!(result_service: peppylib::messaging::ServiceEndpoint));
    }

    if has_feedback {
        // Per-goal feedback: we keep a factory plus an optional active-goal
        // tuple `(goal_id, publisher)` populated by handle_goal_next_request
        // and cleared on result-handled / cancel-accepted / cancel-error.
        fields.push(
            quote!(feedback_publisher_factory: peppylib::messaging::ActionFeedbackPublisherFactory),
        );
        fields.push(
            quote!(current_goal: Option<(String, peppylib::messaging::ActionFeedbackPublisher)>),
        );
    }

    quote! {
        pub struct ActionHandle {
            #( #fields ),*
        }
    }
}

pub fn build_action_expose_method(
    has_goal: bool,
    has_feedback: bool,
    has_result: bool,
) -> TokenStream {
    let mut init_fields = Vec::new();

    if has_goal {
        init_fields.push(quote!(goal_service: action.goal_service));
        init_fields.push(quote!(cancel_service: action.cancel_service));
    }

    if has_result {
        init_fields.push(quote!(result_service: action.result_service));
    }

    if has_feedback {
        init_fields.push(quote!(feedback_publisher_factory: action.feedback_publisher_factory));
        init_fields.push(quote!(current_goal: None));
    }

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            let action = peppylib::ActionMessenger::expose(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                node_runner.processor().node_name(),
                ACTION_NAME,
            )
            .await?;

            Ok(Self {
                #( #init_fields ),*
            })
        }
    }
}

/// What lifecycle role this `handle_*_next_request` plays. Determines
/// whether the generated method extracts a goal_id from the wire payload
/// (`Goal`) or emits a feedback End signal at appropriate boundaries
/// (`Result`, `Cancel`).
#[derive(Clone, Copy)]
pub enum ActionHandleRole {
    /// Service is unrelated to per-goal feedback (action without a feedback
    /// topic, or the goal/result/cancel of such an action).
    Plain,
    /// Goal handler when the action has a feedback topic. Unwraps the
    /// envelope, stores `(goal_id, publisher)` on `self.current_goal` so
    /// later `emit_feedback` calls can target the per-goal topic.
    Goal,
    /// Result handler when the action has a feedback topic. Emits End on
    /// the per-goal feedback topic before awaiting the result request, then
    /// clears `self.current_goal` once the request has been handled.
    Result,
    /// Cancel handler when the action has a feedback topic. Inspects the
    /// user's response: emits End and clears `self.current_goal` if the
    /// cancel was accepted (`accepted == true`) or the user handler
    /// returned an error.
    Cancel,
}

pub fn build_action_handle_method(
    method_name: &Ident,
    helper_name: &Ident,
    request_struct: &Ident,
    response_struct: &Ident,
    service_field: &Ident,
    has_payload: bool,
    role: ActionHandleRole,
) -> TokenStream {
    match role {
        ActionHandleRole::Plain => build_plain_handle_method(
            method_name,
            helper_name,
            request_struct,
            response_struct,
            service_field,
            has_payload,
        ),
        ActionHandleRole::Goal => build_goal_handle_method(
            method_name,
            helper_name,
            request_struct,
            response_struct,
            service_field,
            has_payload,
        ),
        ActionHandleRole::Result => build_result_handle_method(
            method_name,
            helper_name,
            request_struct,
            response_struct,
            service_field,
            has_payload,
        ),
        ActionHandleRole::Cancel => build_cancel_handle_method(
            method_name,
            helper_name,
            request_struct,
            response_struct,
            service_field,
            has_payload,
        ),
    }
}

/// Original behavior: invoke the user's handler via the helper, no
/// lifecycle side effects on `self`.
fn build_plain_handle_method(
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
            let payload = message.payload();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(
                payload.as_ref(),
                &handler,
                core_node,
                instance_id,
            )
        }
    } else {
        quote! {
            let message = request_context.message();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(&handler, core_node, instance_id)
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

/// Goal handler with feedback support: unwrap the envelope, stash goal_id
/// inside the closure, then declare the per-goal publisher after the
/// service call completes. The wire envelope is always present (the
/// codegen wraps even empty user payloads), but the helper's signature
/// depends on whether the action declared a goal request body.
fn build_goal_handle_method(
    method_name: &Ident,
    helper_name: &Ident,
    request_struct: &Ident,
    response_struct: &Ident,
    service_field: &Ident,
    has_payload: bool,
) -> TokenStream {
    let helper_call = if has_payload {
        quote! {
            #helper_name(
                user_payload,
                &handler,
                core_node,
                instance_id,
            )
        }
    } else {
        quote! {
            // user_payload is empty (no goal request body); the helper
            // doesn't take a payload argument.
            let _ = user_payload;
            #helper_name(&handler, core_node, instance_id)
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
            let goal_id_holder: std::sync::Arc<std::sync::Mutex<Option<String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let goal_id_holder_for_closure = std::sync::Arc::clone(&goal_id_holder);

            let result = self
                .#service_field
                .handle_next_request(move |request_context| {
                    let goal_id_holder = std::sync::Arc::clone(&goal_id_holder_for_closure);
                    async move {
                        let message = request_context.message();
                        let wire = message.payload();
                        let (goal_id, user_payload) =
                            peppylib::messaging::unwrap_goal_payload(wire.as_ref())
                                .map_err(|e| peppylib::PeppyError::Io(
                                    std::io::Error::other(e.to_string()),
                                ))?;
                        *goal_id_holder.lock().unwrap() = Some(goal_id.to_string());
                        let core_node = message.core_node().to_string();
                        let instance_id = message.instance_id().to_string();
                        #helper_call
                        .map_err(|error| {
                            peppylib::PeppyError::Io(
                                std::io::Error::other(error.to_string()),
                            )
                        })
                    }
                })
                .await?;

            // Drop the MutexGuard before awaiting `declare()` to avoid
            // clippy::await_holding_lock; the value has already been
            // captured by the closure above so the lock is no longer
            // needed.
            let captured_goal_id = goal_id_holder.lock().unwrap().take();
            if result && let Some(goal_id) = captured_goal_id {
                let publisher = self
                    .feedback_publisher_factory
                    .declare(&goal_id)
                    .await?;
                self.current_goal = Some((goal_id, publisher));
            }
            Ok(result)
        }
    }
}

/// Result handler with feedback support: publish End on the per-goal
/// feedback topic before awaiting the result request (so the client can
/// break out of its drain loop and call `get_result`), then clear the
/// active goal once the request is handled.
fn build_result_handle_method(
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
            let payload = message.payload();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(
                payload.as_ref(),
                &handler,
                core_node,
                instance_id,
            )
        }
    } else {
        quote! {
            let message = request_context.message();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(&handler, core_node, instance_id)
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
            if let Some((_, publisher)) = self.current_goal.as_ref() {
                let _ = publisher.publish_end().await;
            }
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
            self.current_goal = None;
            Ok(result)
        }
    }
}

/// Cancel handler with feedback support: invoke the user's handler, peek
/// at the response (or error), and emit End + clear the active goal when
/// the cancel was accepted or the handler errored. Reject (accepted=false)
/// keeps the feedback stream open since the goal continues.
fn build_cancel_handle_method(
    method_name: &Ident,
    helper_name: &Ident,
    request_struct: &Ident,
    response_struct: &Ident,
    service_field: &Ident,
    has_payload: bool,
) -> TokenStream {
    // The cancel response struct exposes `data.accepted`; we inspect it
    // BEFORE serialization by running the user handler ourselves and
    // forwarding to the existing helper-based serialization path is too
    // restrictive. Instead, emit a small inline closure that calls the
    // helper and threads back whether to close feedback.
    let helper_call = if has_payload {
        quote! {
            let message = request_context.message();
            let payload = message.payload();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(
                payload.as_ref(),
                &handler_for_helper,
                core_node,
                instance_id,
            )
        }
    } else {
        quote! {
            let message = request_context.message();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
            #helper_name(&handler_for_helper, core_node, instance_id)
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
            let publisher = self.current_goal.as_ref().map(|(_, p)| p.clone());
            let close_feedback: std::sync::Arc<std::sync::atomic::AtomicBool> =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let close_feedback_for_closure = std::sync::Arc::clone(&close_feedback);

            // Wrap the user's handler so we can inspect its outcome before
            // the helper serializes the response. CancelResponse is a flat
            // struct (no `.data` nesting on the exposer side) defined by
            // `cancel_action_response_format()` with fields `accepted` and
            // `error_message`.
            let handler = std::sync::Arc::new(handler);
            let handler_for_helper = {
                let handler = std::sync::Arc::clone(&handler);
                let close_feedback = std::sync::Arc::clone(&close_feedback_for_closure);
                move |request: #request_struct| -> crate::Result<#response_struct> {
                    let response = handler(request);
                    let should_close = match &response {
                        Ok(r) => r.accepted,
                        Err(_) => true,
                    };
                    if should_close {
                        close_feedback.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    response
                }
            };
            let publisher_for_closure = publisher.clone();

            let result = self
                .#service_field
                .handle_next_request(move |request_context| {
                    let handler_for_helper = handler_for_helper.clone();
                    let publisher = publisher_for_closure.clone();
                    let close_feedback = std::sync::Arc::clone(&close_feedback_for_closure);
                    async move {
                        let outcome: crate::Result<peppylib::Payload> = {
                            #helper_call
                            .map_err(|error| {
                                peppylib::PeppyError::Io(
                                    std::io::Error::other(error.to_string()),
                                )
                            })
                        };
                        if close_feedback.load(std::sync::atomic::Ordering::SeqCst)
                            && let Some(p) = publisher.as_ref()
                        {
                            let _ = p.publish_end().await;
                        }
                        outcome
                    }
                })
                .await?;

            if close_feedback.load(std::sync::atomic::Ordering::SeqCst) {
                self.current_goal = None;
            }
            Ok(result)
        }
    }
}

pub fn build_action_payload_handler(
    handler_name: &Ident,
    deserializer_name: &Ident,
    request_struct: &Ident,
    request_data_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
    has_payload: bool,
) -> Result<TokenStream> {
    let response_ty = response_spec
        .as_ref()
        .map(|spec| {
            let struct_ident = &spec.struct_ident;
            quote!(#struct_ident)
        })
        .unwrap_or_else(|| quote!(()));

    let request_construction = if request_data_struct.is_some() {
        quote!(let request = #request_struct { instance_id, core_node, data: request_data };)
    } else {
        quote!(let request = #request_struct { instance_id, core_node };)
    };

    let response_serialization = if let Some(spec) = response_spec {
        let response_ident = Ident::new("response", Span::call_site());
        let error_context = quote!(format!("{} {}", stringify!(#handler_name), ACTION_NAME));
        let serialization =
            build_response_payload_tokens(spec, &response_ident, &error_context, None)?;
        let uses_response_data = spec.format.0.iter().any(|(field_name, _)| {
            !(spec.include_service_instance_id && field_name.as_str() == "instance_id")
        });
        let response_stmt = if uses_response_data {
            quote!(let response = handler(request)?;)
        } else {
            quote!(let _ = handler(request)?;)
        };

        quote!({
            #response_stmt
            #serialization
        })
    } else {
        quote!({
            handler(request)?;
            peppylib::Payload::new()
        })
    };

    if has_payload {
        let request_deserialize_stmt = if request_data_struct.is_some() {
            quote!(let request_data = #deserializer_name(payload)?;)
        } else {
            quote!(let () = #deserializer_name(payload)?;)
        };
        Ok(quote! {
            fn #handler_name<F>(
                payload: &[u8],
                handler: &F,
                core_node: String,
                instance_id: String,
            ) -> crate::Result<peppylib::Payload>
            where
                F: Fn(#request_struct) -> crate::Result<#response_ty>,
            {
                #request_deserialize_stmt
                #request_construction

                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        })
    } else {
        Ok(quote! {
            fn #handler_name<F>(
                handler: &F,
                core_node: String,
                instance_id: String,
            ) -> crate::Result<peppylib::Payload>
            where
                F: Fn(#request_struct) -> crate::Result<#response_ty>,
            {
                #request_construction

                let response_payload = #response_serialization;

                Ok(response_payload)
            }
        })
    }
}

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

pub fn build_action_feedback_emit(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    use super::topics::{EmitMethodSpec, build_emit_method};

    let label_literal = Literal::string(label);
    let method_ident = Ident::new("emit_feedback", Span::call_site());

    build_emit_method(EmitMethodSpec {
        method_name: &method_ident,
        params,
        encoding,
        receiver: quote!(&self),
        publish_body: quote! {
            let (_, publisher) = self
                .current_goal
                .as_ref()
                .ok_or_else(|| peppylib::PeppyError::Io(std::io::Error::other(
                    "emit_feedback called with no active goal; \
                     call handle_goal_next_request first"
                )))?;
            publisher.publish(payload).await?;
        },
        error_context: quote!(format!("{} {}", #label_literal, ACTION_NAME)),
        suppress_unused: Vec::new(),
    })
}
