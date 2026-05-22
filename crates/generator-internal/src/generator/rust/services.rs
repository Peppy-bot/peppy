use super::context::SchemaFieldLookup;
use super::deserialization::{build_deserialize_fn, generate_field_reader_statements};
use super::identifiers::sanitize_rust_identifier;
use super::serialization::{
    MessageEncodingSpec, NameGenerator, build_serialize_payload, generate_field_assignment,
};
use super::topics::sender_target_expression;
use crate::error::{Error, Result};
use crate::generator::types::InterfaceOrigin;
use config::encoding::FunctionParam;
use config::node::MessageFormat;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

pub struct ServiceResponseSpec<'a> {
    pub format: &'a MessageFormat,
    pub struct_ident: Ident,
    pub builder_type: TokenStream,
    pub include_service_instance_id: bool,
}

#[derive(Clone, Copy)]
pub struct ExposedServiceMethodSpec<'a> {
    pub fn_name: &'a Ident,
    pub handler_fn_name_override: Option<&'a Ident>,
    pub handler_helper_name_override: Option<&'a Ident>,
    pub request_deserializer_name_override: Option<&'a Ident>,
    pub wire_params: &'a [FunctionParam],
    pub handler_params: &'a [FunctionParam],
    pub instance_id_param: Option<&'a FunctionParam>,
    pub encoding: Option<&'a MessageEncodingSpec>,
    pub request_format: Option<&'a MessageFormat>,
    pub label: &'a str,
    pub service_name_literal: &'a Literal,
    pub request_struct: Option<&'a Ident>,
    pub request_data_struct: Option<&'a Ident>,
    pub response_spec: Option<&'a ServiceResponseSpec<'a>>,
    pub use_service_name_const: bool,
    /// `Some(o)` when the service is conformed via `interfaces.conforms_to`;
    /// `None` for native services. Drives the `iface_name`/`iface_tag` segments
    /// spliced into the generated `ServiceMessenger::listen` call.
    pub origin: Option<&'a InterfaceOrigin>,
}

pub fn build_exposed_service_method(
    spec: &ExposedServiceMethodSpec,
) -> Result<(TokenStream, Vec<TokenStream>)> {
    let ExposedServiceMethodSpec {
        fn_name,
        handler_fn_name_override,
        handler_helper_name_override,
        request_deserializer_name_override,
        wire_params,
        handler_params,
        instance_id_param,
        encoding,
        request_format,
        label,
        service_name_literal,
        request_struct,
        request_data_struct,
        response_spec,
        use_service_name_const,
        origin,
    } = *spec;
    let target_expr = sender_target_expression(origin);

    let handler_fn_name = handler_fn_name_override.cloned().unwrap_or_else(|| {
        Ident::new(
            &format!("handle_{}_next_request", fn_name),
            Span::call_site(),
        )
    });

    let callback_param_types: Vec<TokenStream> = if use_service_name_const {
        if let Some(request_struct) = request_struct {
            vec![quote!(#request_struct)]
        } else {
            Vec::new()
        }
    } else {
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
    )?;
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

    let has_payload = encoding.is_some();
    let mut helper_tokens = Vec::new();

    // Build helper_params once (shared across encoding/no-encoding paths).
    let mut helper_params: Vec<TokenStream> = Vec::new();
    if has_payload {
        helper_params.push(quote!(payload: &[u8]));
    }
    helper_params.push(quote!(handler: &F));
    if use_service_name_const {
        helper_params.push(quote!(core_node: String));
        helper_params.push(quote!(instance_id: String));
    } else if instance_from_request_context {
        let instance_ident =
            instance_binding_ident
                .as_ref()
                .ok_or_else(|| Error::InvariantViolation {
                    context: String::from(
                        "instance_id param should exist when provided from context",
                    ),
                })?;
        helper_params.push(quote!(#instance_ident: String));
    }
    if let Some(instance_ident) = service_instance_param_ident.as_ref() {
        helper_params.push(quote!(#instance_ident: &str));
    }

    // Build helper function body preamble from independent concerns.
    let mut body_preamble: Vec<TokenStream> = Vec::new();

    if let Some(request_spec) = encoding {
        let request_format = request_format.ok_or_else(|| Error::InvariantViolation {
            context: String::from("request format should exist when encoding is present"),
        })?;

        let deserializer_struct = if use_service_name_const {
            request_data_struct
        } else {
            request_struct
        };
        let instance_id_for_deserializer = if instance_from_request_context {
            None
        } else {
            instance_id_param
        };

        let request_deserializer = build_request_deserializer(&RequestDeserializerSpec {
            deserializer_fn_name: &request_deserializer_name,
            request_spec,
            request_format,
            wire_params,
            handler_params,
            label,
            request_struct: deserializer_struct,
            instance_id_param: instance_id_for_deserializer,
            use_service_name_const,
        })?;
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

        body_preamble
            .push(quote!(let #deserializer_pattern = #request_deserializer_name(payload)?;));
    }

    if use_service_name_const {
        let request_construction = if has_payload && request_data_struct.is_some() {
            quote!(let request = Request { instance_id, core_node, data: request_data };)
        } else {
            quote!(let request = Request { instance_id, core_node };)
        };
        body_preamble.push(request_construction);
    }

    let helper_fn = quote! {
        fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<peppylib::Payload>
        where
            F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
        {
            #(#body_preamble)*

            let response_payload = #response_serialization;

            Ok(response_payload)
        }
    };
    helper_tokens.push(helper_fn);

    // Build helper call tokens by composing 3 independent concerns.
    let request_context_ident = if has_payload || instance_from_request_context {
        Ident::new("request_context", Span::call_site())
    } else {
        Ident::new("_request_context", Span::call_site())
    };

    let needs_message_binding = use_service_name_const || instance_from_request_context;
    let mut call_preamble: Vec<TokenStream> = Vec::new();
    let mut helper_args: Vec<TokenStream> = Vec::new();

    if needs_message_binding {
        call_preamble.push(quote!(let message = #request_context_ident.message();));
        // Extract payload from message when encoding is present, or when
        // instance_from_request_context without use_service_name_const (preserves
        // existing generated code shape).
        if has_payload || (instance_from_request_context && !use_service_name_const) {
            call_preamble.push(quote!(let payload = message.payload();));
        }
    } else if has_payload {
        call_preamble.push(quote!(let payload = #request_context_ident.message().payload();));
    }

    if has_payload {
        helper_args.push(quote!(payload.as_ref()));
    }
    helper_args.push(quote!(&handler));

    if use_service_name_const {
        call_preamble.push(quote!(let core_node = message.core_node().to_string();));
        call_preamble.push(quote!(let instance_id = message.instance_id().to_string();));
        helper_args.push(quote!(core_node));
        helper_args.push(quote!(instance_id));
    } else if instance_from_request_context {
        call_preamble.push(quote!(let instance_id = message.instance_id().to_string();));
        helper_args.push(quote!(instance_id));
    }

    if let Some(arg) = &service_instance_call_arg {
        helper_args.push(arg.clone());
    }

    let helper_call_tokens = if call_preamble.is_empty() {
        quote!(#handler_helper_name(#(#helper_args),*))
    } else {
        quote!({
            #(#call_preamble)*
            #handler_helper_name(#(#helper_args),*)
        })
    };

    let service_name_ref = if use_service_name_const {
        quote!(SERVICE_NAME)
    } else {
        quote!(service_name)
    };

    let method = if use_service_name_const {
        quote! {
            pub async fn #handler_fn_name<F>(
                node_runner: &crate::NodeRunner,
                handler: F,
            ) -> crate::Result<()>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                let mut service = peppylib::ServiceMessenger::listen(
                    node_runner.messenger(),
                    node_runner.processor().bound_core_node(),
                    node_runner.processor().bound_instance_id(),
                    #target_expr,
                    node_runner.processor().link_ids(),
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
                node_runner: &crate::NodeRunner,
                handler: F,
            ) -> crate::Result<()>
            where
                F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
            {
                #service_instance_env_stmt
                #service_name_binding

                let mut service = peppylib::ServiceMessenger::listen(
                    node_runner.messenger(),
                    node_runner.core_node(),
                    service_instance_id.as_str(),
                    #target_expr,
                    node_runner.processor().link_ids(),
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

    Ok((method, helper_tokens))
}

pub fn build_request_struct_with_name_and_impl(
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

#[derive(Clone, Copy)]
pub struct RequestDeserializerSpec<'a> {
    pub deserializer_fn_name: &'a Ident,
    pub request_spec: &'a MessageEncodingSpec,
    pub request_format: &'a MessageFormat,
    pub wire_params: &'a [FunctionParam],
    pub handler_params: &'a [FunctionParam],
    pub label: &'a str,
    pub request_struct: Option<&'a Ident>,
    pub instance_id_param: Option<&'a FunctionParam>,
    pub use_service_name_const: bool,
}

pub fn build_request_deserializer(spec: &RequestDeserializerSpec) -> Result<TokenStream> {
    let RequestDeserializerSpec {
        deserializer_fn_name,
        request_spec,
        request_format,
        wire_params,
        handler_params,
        label,
        request_struct,
        instance_id_param,
        use_service_name_const,
    } = *spec;

    let field_context_expr = if use_service_name_const {
        quote!(String::from(SERVICE_NAME))
    } else {
        let context_literal = Literal::string(label);
        quote!(String::from(#context_literal))
    };

    let reader_type = &request_spec.reader_type;

    if let Some(instance_param) = instance_id_param {
        let instance_ty = &instance_param.ty;
        let request_return_ty = build_return_type_from_params(handler_params, request_struct);
        let return_ty = quote!((#instance_ty, #request_return_ty));

        let schema_lookup = SchemaFieldLookup::new(request_format)?;
        let mut names = NameGenerator::new();
        let mut field_statements = Vec::new();
        let mut handler_values: Vec<(String, Ident)> = Vec::with_capacity(wire_params.len());
        let mut instance_value_ident = None;
        let instance_field_key = instance_param.ident.to_string();

        for param in wire_params {
            let field_key = param.ident.to_string();
            let (original_name, schema) = schema_lookup.get(&field_key)?;

            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                original_name.as_str(),
                schema,
                label,
                &field_context_expr,
                &mut names,
            )?;
            field_statements.append(&mut statements);

            if field_key == instance_field_key {
                instance_value_ident = Some(value_ident);
            } else {
                handler_values.push((field_key, value_ident));
            }
        }

        let instance_value_ident =
            instance_value_ident.ok_or_else(|| Error::InvariantViolation {
                context: String::from("instance_id should be present in service requests"),
            })?;

        let mut ordered_request_values: Vec<Ident> = Vec::with_capacity(handler_params.len());
        for param in handler_params {
            let key = param.ident.to_string();
            let value_ident = handler_values
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Error::InvariantViolation {
                    context: format!("missing field `{key}` in request payload"),
                })?;
            ordered_request_values.push(value_ident);
        }

        let request_expr =
            build_result_expr_from_values(handler_params, &ordered_request_values, request_struct);
        let result_expr = quote!((#instance_value_ident, #request_expr));

        Ok(build_deserialize_fn(
            deserializer_fn_name,
            reader_type,
            &field_context_expr,
            &return_ty,
            &field_statements,
            &result_expr,
        ))
    } else {
        let return_ty = build_return_type_from_params(handler_params, request_struct);
        let (field_statements, value_idents) = deserialize_fields_from_format(
            request_format,
            wire_params,
            label,
            &field_context_expr,
        )?;
        let request_expr =
            build_result_expr_from_values(handler_params, &value_idents, request_struct);

        Ok(build_deserialize_fn(
            deserializer_fn_name,
            reader_type,
            &field_context_expr,
            &return_ty,
            &field_statements,
            &request_expr,
        ))
    }
}

pub fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    label: &str,
    callback_call: &TokenStream,
    service_instance_ident: Option<&Ident>,
    use_service_name_const: bool,
) -> Result<TokenStream> {
    let Some(spec) = response_spec else {
        return Ok(quote!({
            #callback_call?;
            peppylib::Payload::new()
        }));
    };

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
    )?;

    let uses_response_data = spec.format.0.iter().any(|(field_name, _)| {
        !(spec.include_service_instance_id && field_name.as_str() == "instance_id")
    });
    let response_stmt = if uses_response_data {
        quote!(let response = #callback_call?;)
    } else {
        quote!(let _ = #callback_call?;)
    };

    Ok(quote!({
        #response_stmt
        #serialization
    }))
}

pub fn build_return_type_from_params(
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

pub fn deserialize_fields_from_format(
    request_format: &MessageFormat,
    params: &[FunctionParam],
    label: &str,
    context_expr: &TokenStream,
) -> Result<(Vec<TokenStream>, Vec<Ident>)> {
    let schema_lookup = SchemaFieldLookup::new(request_format)?;
    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut value_idents = Vec::with_capacity(params.len());

    for param in params {
        let field_key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&field_key)?;

        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            original_name.as_str(),
            schema,
            label,
            context_expr,
            &mut names,
        )?;
        field_statements.append(&mut statements);
        value_idents.push(value_ident);
    }

    Ok((field_statements, value_idents))
}

pub fn build_result_expr_from_values(
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

pub fn build_response_payload_tokens(
    spec: &ServiceResponseSpec,
    response_ident: &Ident,
    error_context: &TokenStream,
    service_instance_ident: Option<&Ident>,
) -> Result<TokenStream> {
    let format = spec.format;
    let builder_ident = Ident::new("root", Span::call_site());

    let mut assignments = Vec::with_capacity(format.0.len());
    let mut names = NameGenerator::new();

    for (field_name, schema) in &format.0 {
        if spec.include_service_instance_id && field_name == "instance_id" {
            let instance_ident =
                service_instance_ident.ok_or_else(|| Error::InvariantViolation {
                    context: String::from(
                        "service instance identifier should be available when required",
                    ),
                })?;
            assignments.push(quote!(#builder_ident.set_instance_id(#instance_ident);));
            continue;
        }

        let field_ident = Ident::new(
            &sanitize_rust_identifier(field_name.as_str()),
            Span::call_site(),
        );
        let value_expr = quote!(#response_ident.#field_ident);
        assignments.push(generate_field_assignment(
            &quote!(#builder_ident),
            field_name,
            schema,
            &value_expr,
            &mut names,
        )?);
    }

    Ok(build_serialize_payload(
        &spec.builder_type,
        &[],
        &assignments,
        error_context,
    ))
}
