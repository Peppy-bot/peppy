use super::deserialization::build_deserialize_fn;
use super::serialization::MessageEncodingSpec;
use super::services::{
    ServiceResponseSpec, build_response_payload_tokens, build_result_expr_from_values,
    build_return_type_from_params, deserialize_fields_from_format,
};
use super::topics::{EmitMethodSpec, build_emit_method};
use crate::error::Result;
use config::encoding::FunctionParam;
use config::node::MessageFormat;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

/// The server `ActionHandle` wraps the concurrent [`peppylib::messaging::ActionServer`].
/// It vends one `GoalContext` per accepted goal; the context owns that goal's
/// feedback publisher, cancel signal, and result delivery, so a server can drive
/// many goals concurrently.
pub fn build_action_handle_struct() -> TokenStream {
    quote! {
        pub struct ActionHandle {
            server: peppylib::messaging::ActionServer,
        }
    }
}

pub fn build_action_expose_method(
    origin: Option<&crate::generator::types::InterfaceOrigin>,
) -> TokenStream {
    let target_expr = super::topics::sender_target_expression(origin);

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            let server = peppylib::ActionMessenger::expose(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #target_expr,
                ACTION_NAME,
            )
            .await?;

            Ok(Self { server })
        }
    }
}

/// `handle_goal_next_request`: one step of the accept loop. Waits for the next
/// goal, decodes the typed request, and runs the user's `decide` closure. On
/// `Ok` it registers a [`GoalContext`] (inserting the routing slot before
/// replying, so a fast cancel/result can't miss it), replies with the goal
/// response, and returns the context. On `Err` it rejects the goal and returns
/// `None`; it also returns `None` when the goal service has closed.
///
/// `has_request_data` selects whether `handle_goal_payload` takes the decoded
/// request body; `has_response` selects the user closure's response type.
pub fn build_action_goal_method(has_request_data: bool, has_response: bool) -> TokenStream {
    let response_ty = if has_response {
        quote!(GoalResponse)
    } else {
        quote!(())
    };

    // `handle_goal_payload` (built by `build_action_payload_handler`) decodes the
    // request, calls our capture wrapper, and serializes the response.
    let handler_call = if has_request_data {
        quote!(handle_goal_payload(
            &user_payload,
            &capture,
            core_node,
            instance_id
        ))
    } else {
        quote!({
            let _ = &user_payload;
            handle_goal_payload(&capture, core_node, instance_id)
        })
    };

    quote! {
        pub async fn handle_goal_next_request<F>(
            &mut self,
            decide: F,
        ) -> crate::Result<Option<GoalContext>>
        where
            F: Fn(&GoalRequest) -> crate::Result<#response_ty>,
        {
            let Some((request_context, responder)) = self.server.recv_next_goal().await? else {
                return Ok(None);
            };

            let core_node = request_context.message().core_node().to_string();
            let instance_id = request_context.message().instance_id().to_string();
            let user_payload = {
                let wire = request_context.message().payload();
                let (_goal_id, body) = peppylib::messaging::unwrap_goal_payload(wire.as_ref())
                    .map_err(|error| {
                        peppylib::PeppyError::Io(std::io::Error::other(error.to_string()))
                    })?;
                body.to_vec()
            };

            // The capture wrapper runs the user's decision and stashes the
            // decoded request so an accepted goal can hand it to the context.
            let captured: std::cell::RefCell<Option<GoalRequest>> = std::cell::RefCell::new(None);
            let capture = |request: GoalRequest| -> crate::Result<#response_ty> {
                let outcome = decide(&request);
                *captured.borrow_mut() = Some(request);
                outcome
            };

            let outcome: crate::Result<peppylib::Payload> = #handler_call;
            match outcome {
                Ok(response_payload) => {
                    let inner = self.server.register_goal(&request_context).await?;
                    let _ = responder.respond(response_payload).await;
                    let request = captured
                        .into_inner()
                        .expect("goal request captured by the decision closure");
                    Ok(Some(GoalContext { inner, request }))
                }
                Err(error) => {
                    let _ = responder.respond_error(error.to_string()).await;
                    Ok(None)
                }
            }
        }
    }
}

/// The per-goal `GoalContext` type and its impl. `context_methods` is the set
/// of action-specific methods (`publish_feedback`, `complete`) appended to the
/// always-present accessors.
pub fn build_goal_context(context_methods: Vec<TokenStream>) -> TokenStream {
    quote! {
        /// Per-goal handle returned by `ActionHandle::handle_goal_next_request`.
        /// Owns this goal's feedback stream, cancel signal, and result delivery;
        /// move it into a spawned task to drive the goal to completion.
        pub struct GoalContext {
            inner: peppylib::messaging::GoalContext,
            request: GoalRequest,
        }

        impl GoalContext {
            /// The decoded goal request this context is driving.
            pub fn request(&self) -> &GoalRequest {
                &self.request
            }

            /// The client-generated id of this goal.
            pub fn goal_id(&self) -> &str {
                self.inner.goal_id()
            }

            /// Whether a cancel for this goal has been received.
            pub fn is_cancelled(&self) -> bool {
                self.inner.is_cancelled()
            }

            /// Resolves when a cancel for this goal arrives. Idempotent and safe
            /// to await inside a `tokio::select!`.
            pub async fn cancel_signal(&self) {
                self.inner.cancel_signal().await
            }

            #( #context_methods )*
        }
    }
}

/// The `publish_feedback` method on `GoalContext`: serializes the typed feedback
/// fields and publishes them on this goal's stream.
pub fn build_action_publish_feedback(
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
            let payload = peppylib::messaging::NonEmptyPayload::try_new(payload).map_err(|_| {
                peppylib::PeppyError::Io(std::io::Error::other(
                    "publish_feedback produced an empty payload (codec serialized \
                     to zero bytes); empty is reserved for the end-of-stream sentinel",
                ))
            })?;
            self.inner.publish_feedback(payload).await?;
        },
        error_context: quote!(format!("{} {}", #label_literal, ACTION_NAME)),
        suppress_unused: Vec::new(),
    })
}

/// The `complete` method on `GoalContext`: serializes the typed result fields
/// and delivers them, rendezvousing with the client's `get_result`. When the
/// result has no fields it sends an empty payload.
pub fn build_action_complete_method(
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
) -> TokenStream {
    match encoding {
        Some(_) => {
            let method_ident = Ident::new("complete", Span::call_site());
            build_emit_method(EmitMethodSpec {
                method_name: &method_ident,
                params,
                encoding,
                receiver: quote!(&self),
                publish_body: quote! {
                    self.inner.complete(payload).await?;
                },
                error_context: quote!(format!("complete {}", ACTION_NAME)),
                suppress_unused: Vec::new(),
            })
        }
        None => quote! {
            pub async fn complete(&self) -> crate::Result<()> {
                self.inner.complete(peppylib::Payload::new()).await?;
                Ok(())
            }
        },
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
