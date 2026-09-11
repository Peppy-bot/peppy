use super::context::SchemaFieldLookup;
use super::deserialization::{build_deserialize_fn, generate_field_reader_statements};
use super::identifiers::sanitize_rust_identifier;
use super::serialization::{
    MessageEncodingSpec, NameGenerator, build_serialize_payload, generate_field_assignment,
};
use super::topics::sender_target_expression;
use crate::error::{Error, Result};
use crate::generator::types::ContractOrigin;
use config::node::MessageFormat;
use encoding::FunctionParam;
use proc_macro2::{Ident, Span, TokenStream};
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
    pub encoding: Option<&'a MessageEncodingSpec>,
    pub request_format: Option<&'a MessageFormat>,
    /// The prefix the request's nested message structs are defined with, so
    /// the deserializer constructs the types the module declares.
    pub struct_prefix: &'a str,
    pub request_struct: Option<&'a Ident>,
    pub request_data_struct: Option<&'a Ident>,
    pub response_spec: Option<&'a ServiceResponseSpec<'a>>,
    /// `Some(o)` when the service is contract-backed via `manifest.implements`;
    /// `None` for native services. Drives the `contract_name`/`contract_tag` segments
    /// spliced into the generated `ServiceMessenger::listen` call.
    pub origin: Option<&'a ContractOrigin>,
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
        encoding,
        request_format,
        struct_prefix,
        request_struct,
        request_data_struct,
        response_spec,
        origin,
    } = *spec;
    let target_expr = sender_target_expression(origin);

    let handler_fn_name = handler_fn_name_override.cloned().unwrap_or_else(|| {
        Ident::new(
            &format!("handle_{}_next_request", fn_name),
            Span::call_site(),
        )
    });

    let callback_param_types: Vec<TokenStream> = request_struct
        .map(|request_struct| vec![quote!(#request_struct)])
        .unwrap_or_default();

    let response_ty = response_spec
        .as_ref()
        .map(|spec| {
            let struct_ident = &spec.struct_ident;
            quote!(#struct_ident)
        })
        .unwrap_or_else(|| quote!(()));

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

    let request_ident = Ident::new("request", Span::call_site());
    let callback_call = quote!(handler(#request_ident));
    let request_pattern = if request_data_struct.is_some() {
        let binding_ident = Ident::new("request_data", Span::call_site());
        quote!(#binding_ident)
    } else {
        quote!(())
    };

    let response_serialization = build_response_serialization_code(
        response_spec,
        &callback_call,
        service_instance_param_ident.as_ref(),
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
    helper_params.push(quote!(core_node: String));
    helper_params.push(quote!(instance_id: String));
    if let Some(instance_ident) = service_instance_param_ident.as_ref() {
        helper_params.push(quote!(#instance_ident: &str));
    }

    // Build helper function body preamble from independent concerns.
    let mut body_preamble: Vec<TokenStream> = Vec::new();

    if let Some(request_spec) = encoding {
        let request_format = request_format.ok_or_else(|| Error::InvariantViolation {
            context: String::from("request format should exist when encoding is present"),
        })?;

        let request_deserializer = build_request_deserializer(&RequestDeserializerSpec {
            deserializer_fn_name: &request_deserializer_name,
            request_spec,
            request_format,
            wire_params,
            handler_params,
            struct_prefix,
            request_struct: request_data_struct,
        })?;
        helper_tokens.push(request_deserializer);

        body_preamble.push(quote!(let #request_pattern = #request_deserializer_name(payload)?;));
    }

    let request_construction = if has_payload && request_data_struct.is_some() {
        quote!(let request = Request { instance_id, core_node, data: request_data };)
    } else {
        quote!(let request = Request { instance_id, core_node };)
    };
    body_preamble.push(request_construction);

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

    // The helper call: the message's identity, its payload when the service
    // takes one, and the service instance when the response names it.
    let request_context_ident = Ident::new("request_context", Span::call_site());
    let mut call_preamble: Vec<TokenStream> = Vec::new();
    let mut helper_args: Vec<TokenStream> = Vec::new();

    call_preamble.push(quote!(let message = #request_context_ident.message();));
    if has_payload {
        call_preamble.push(quote!(let payload = message.payload_bytes();));
        helper_args.push(quote!(payload.as_ref()));
    }
    helper_args.push(quote!(&handler));
    call_preamble.push(quote!(let core_node = message.core_node().to_string();));
    call_preamble.push(quote!(let instance_id = message.instance_id().to_string();));
    helper_args.push(quote!(core_node));
    helper_args.push(quote!(instance_id));

    if let Some(arg) = &service_instance_call_arg {
        helper_args.push(arg.clone());
    }

    let helper_call_tokens = quote!({
        #(#call_preamble)*
        #handler_helper_name(#(#helper_args),*)
    });

    let service_name_ref = quote!(SERVICE_NAME);

    let method = quote! {
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
    pub struct_prefix: &'a str,
    pub request_struct: Option<&'a Ident>,
}

pub fn build_request_deserializer(spec: &RequestDeserializerSpec) -> Result<TokenStream> {
    let RequestDeserializerSpec {
        deserializer_fn_name,
        request_spec,
        request_format,
        wire_params,
        handler_params,
        struct_prefix,
        request_struct,
    } = *spec;

    let field_context_expr = quote!(String::from(SERVICE_NAME));
    let reader_type = &request_spec.reader_type;

    let return_ty = build_return_type_from_params(handler_params, request_struct);
    let (field_statements, value_idents) = deserialize_fields_from_format(
        request_format,
        wire_params,
        struct_prefix,
        &field_context_expr,
    )?;
    let request_expr = build_result_expr_from_values(handler_params, &value_idents, request_struct);

    Ok(build_deserialize_fn(
        deserializer_fn_name,
        reader_type,
        &field_context_expr,
        &return_ty,
        &field_statements,
        &request_expr,
    ))
}

pub fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    callback_call: &TokenStream,
    service_instance_ident: Option<&Ident>,
) -> Result<TokenStream> {
    let Some(spec) = response_spec else {
        return Ok(quote!({
            #callback_call?;
            peppylib::Payload::new()
        }));
    };

    let error_context = quote!(format!("handle_request_payload {}", SERVICE_NAME));
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
    struct_prefix: &str,
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
            struct_prefix,
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
