use super::deserialization::build_deserialize_fn;
use super::serialization::{MessageEncodingSpec, build_serialize_payload};
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
        fields.push(quote!(feedback_publisher: peppylib::messaging::TopicPublisher));
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
        init_fields.push(quote!(feedback_publisher: action.feedback_publisher));
    }

    quote! {
        pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self> {
            let action = peppylib::ActionMessenger::expose(
                node_runner.messenger(),
                node_runner.processor().bound_master_node(),
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

pub fn build_action_handle_method(
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
        quote!(let request = #request_struct { instance_id, master_node, data: request_data };)
    } else {
        quote!(let request = #request_struct { instance_id, master_node };)
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
            bytes::Bytes::new()
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
                master_node: String,
                instance_id: String,
            ) -> crate::Result<bytes::Bytes>
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
    let mut method_param_tokens = Vec::new();
    let instance_id_ident = Ident::new("instance_id", Span::call_site());
    for param in params {
        if param.ident == instance_id_ident {
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
            let error_context = quote!(format!("{} {}", #label_literal, ACTION_NAME));
            let serialize_block =
                build_serialize_payload(&spec.builder_type, &[], &spec.assignments, &error_context);

            quote! {
                #[allow(clippy::too_many_arguments)]
                pub async fn emit_feedback(#method_signature) -> crate::Result<()> {
                    let payload = #serialize_block;
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
