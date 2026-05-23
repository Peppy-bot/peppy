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
    origin: Option<&crate::generator::types::InterfaceOrigin>,
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

    let target_expr = super::topics::sender_target_expression(origin);

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            // The producer always declares its queryables under the reserved
            // default `_` link_id segment; consumers pin by
            // `target_instance_id` derived from the binding map.
            let action = peppylib::ActionMessenger::expose(
                node_runner.messenger(),
                node_runner.processor().bound_core_node(),
                node_runner.processor().bound_instance_id(),
                #target_expr,
                &[],
                ACTION_NAME,
            )
            .await?;

            Ok(Self {
                #( #init_fields ),*
            })
        }
    }
}

/// Lifecycle role of a `handle_*_next_request` method. The non-Plain
/// variants extract or emit per-goal feedback signals around the user
/// handler; Plain just dispatches.
#[derive(Clone, Copy)]
pub enum ActionHandleRole {
    Plain,
    /// Unwrap the goal envelope and stash `(goal_id, publisher)` on
    /// `self.current_goal` for subsequent `emit_feedback` calls.
    Goal,
    /// Publish the end-of-stream sentinel on the active goal's feedback
    /// topic before serving the result request, then clear `current_goal`.
    Result,
    /// On `accepted == true` or handler error, publish end-of-stream and
    /// clear `current_goal`. A rejection keeps the feedback stream open
    /// since the goal continues.
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
    let setup = match role {
        ActionHandleRole::Plain => quote!(),
        ActionHandleRole::Goal => quote! {
            type Captured = (String, peppylib::messaging::ActionFeedbackPublisher);
            let captured: std::sync::Arc<std::sync::Mutex<Option<Captured>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let captured_for_closure = std::sync::Arc::clone(&captured);
            let factory = self.feedback_publisher_factory.clone();
        },
        ActionHandleRole::Result => quote! {
            if let Some((_, publisher)) = self.current_goal.as_ref() {
                let _ = publisher.publish_end().await;
            }
        },
        ActionHandleRole::Cancel => quote! {
            let publisher = self.current_goal.as_ref().map(|(_, p)| p.clone());
            let close_decision: std::sync::Arc<std::sync::Mutex<Option<bool>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let close_decision_for_closure = std::sync::Arc::clone(&close_decision);
            // CancelResponse.accepted decides whether to publish end-of-stream.
            // Inspecting it requires running the user handler ourselves rather
            // than letting the helper serialize directly.
            let handler_for_helper = {
                let close_decision = std::sync::Arc::clone(&close_decision_for_closure);
                move |request: #request_struct| -> crate::Result<#response_struct> {
                    let response = handler(request);
                    let should_close = match &response {
                        Ok(r) => r.accepted,
                        Err(_) => true,
                    };
                    *close_decision.lock().unwrap() = Some(should_close);
                    response
                }
            };
        },
    };

    let payload_setup = match role {
        ActionHandleRole::Goal => quote! {
            let message = request_context.message();
            let goal_link_id = request_context.link_id().to_string();
            let wire = message.payload().into_inner();
            let declared = factory.declare_from_wire(&goal_link_id, wire).await?;
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
        },
        _ if has_payload => quote! {
            let message = request_context.message();
            let payload = message.payload();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
        },
        _ => quote! {
            let message = request_context.message();
            let core_node = message.core_node().to_string();
            let instance_id = message.instance_id().to_string();
        },
    };

    let handler_ref = match role {
        ActionHandleRole::Cancel => quote!(&handler_for_helper),
        _ => quote!(&handler),
    };
    let helper_call = match (role, has_payload) {
        (ActionHandleRole::Goal, true) => quote!(#helper_name(
            declared.user_payload.as_ref(),
            #handler_ref,
            core_node,
            instance_id,
        )),
        (ActionHandleRole::Goal, false) => quote!({
            let _ = declared.user_payload;
            #helper_name(#handler_ref, core_node, instance_id)
        }),
        (_, true) => quote!(#helper_name(
            payload.as_ref(),
            #handler_ref,
            core_node,
            instance_id,
        )),
        (_, false) => quote!(#helper_name(#handler_ref, core_node, instance_id)),
    };

    let post_outcome = match role {
        ActionHandleRole::Goal => quote! {
            if outcome.is_ok() {
                *captured_for_closure.lock().unwrap() =
                    Some((declared.goal_id, declared.publisher));
            }
        },
        ActionHandleRole::Cancel => quote! {
            if matches!(*close_decision_for_closure.lock().unwrap(), Some(true))
                && let Some(p) = publisher.as_ref()
            {
                let _ = p.publish_end().await;
            }
        },
        _ => quote!(),
    };

    let post_call = match role {
        ActionHandleRole::Plain => quote!(),
        ActionHandleRole::Goal => quote! {
            if let Some(active_goal) = captured.lock().unwrap().take() {
                self.current_goal = Some(active_goal);
            }
        },
        ActionHandleRole::Result => quote! {
            self.current_goal = None;
        },
        ActionHandleRole::Cancel => quote! {
            if matches!(*close_decision.lock().unwrap(), Some(true)) {
                self.current_goal = None;
            }
        },
    };

    quote! {
        pub async fn #method_name<F>(
            &mut self,
            handler: F,
        ) -> crate::Result<bool>
        where
            F: Fn(#request_struct) -> crate::Result<#response_struct>,
        {
            #setup
            let result = self
                .#service_field
                .handle_next_request(|request_context| async move {
                    #payload_setup
                    let outcome: crate::Result<peppylib::Payload> = #helper_call
                        .map_err(|error| {
                            peppylib::PeppyError::Io(
                                std::io::Error::other(error.to_string()),
                            )
                        });
                    #post_outcome
                    outcome
                })
                .await?;
            #post_call
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
            let payload = peppylib::messaging::NonEmptyPayload::try_new(payload)
                .map_err(|_| peppylib::PeppyError::Io(std::io::Error::other(
                    "emit_feedback produced an empty payload (codec serialized \
                     to zero bytes); empty is reserved for publish_end"
                )))?;
            publisher.publish(payload).await?;
        },
        error_context: quote!(format!("{} {}", #label_literal, ACTION_NAME)),
        suppress_unused: Vec::new(),
    })
}
