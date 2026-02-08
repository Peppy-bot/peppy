use super::context::SchemaFieldLookup;
use super::deserialization::generate_field_reader_statements;
use super::serialization::{MessageEncodingSpec, NameGenerator, generate_field_assignment};
use crate::generator::naming::sanitize_component;
use config::encoding::FunctionParam;
use config::node::MessageFormat;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;

pub(super) struct ServiceResponseSpec<'a> {
    pub(super) format: &'a MessageFormat,
    pub(super) struct_ident: Ident,
    pub(super) builder_type: TokenStream,
    pub(super) include_service_instance_id: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_exposed_service_method(
    fn_name: &Ident,
    handler_fn_name_override: Option<&Ident>,
    handler_helper_name_override: Option<&Ident>,
    request_deserializer_name_override: Option<&Ident>,
    wire_params: &[FunctionParam],
    handler_params: &[FunctionParam],
    instance_id_param: Option<&FunctionParam>,
    encoding: Option<&MessageEncodingSpec>,
    request_format: Option<&MessageFormat>,
    label: &str,
    service_name_literal: &Literal,
    request_struct: Option<&Ident>,
    request_data_struct: Option<&Ident>,
    response_spec: Option<&ServiceResponseSpec>,
    use_service_name_const: bool,
) -> (TokenStream, Vec<TokenStream>) {
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
    );
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

    let mut helper_tokens = Vec::new();

    if let Some(request_spec) = encoding {
        let request_format =
            request_format.expect("request format should exist when encoding is present");

        let deserializer_struct = if use_service_name_const {
            request_data_struct
        } else {
            request_struct
        };

        let request_deserializer = if instance_from_request_context {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                deserializer_struct,
                None,
                use_service_name_const,
            )
        } else {
            build_request_deserializer(
                &request_deserializer_name,
                request_spec,
                request_format,
                wire_params,
                handler_params,
                label,
                deserializer_struct,
                instance_id_param,
                use_service_name_const,
            )
        };
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

        let mut helper_params: Vec<TokenStream> = vec![quote!(payload: &[u8]), quote!(handler: &F)];

        if use_service_name_const {
            helper_params.push(quote!(master_node: String));
            helper_params.push(quote!(instance_id: String));
        } else if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = if use_service_name_const {
            let request_construction = if request_data_struct.is_some() {
                quote!(let request = Request { instance_id, master_node, data: request_data };)
            } else {
                quote!(let request = Request { instance_id, master_node };)
            };
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let #deserializer_pattern = #request_deserializer_name(payload)?;
                    #request_construction

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        } else {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let #deserializer_pattern = #request_deserializer_name(payload)?;

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        };
        helper_tokens.push(helper_fn);
    } else {
        let mut helper_params: Vec<TokenStream> = vec![quote!(handler: &F)];

        if use_service_name_const {
            helper_params.push(quote!(master_node: String));
            helper_params.push(quote!(instance_id: String));
        } else if instance_from_request_context {
            let instance_ident = instance_binding_ident
                .as_ref()
                .expect("instance_id param should exist when provided from context");
            helper_params.push(quote!(#instance_ident: String));
        }

        if let Some(instance_ident) = service_instance_param_ident.as_ref() {
            helper_params.push(quote!(#instance_ident: &str));
        }

        let helper_fn = if use_service_name_const {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let request = Request { instance_id, master_node };

                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        } else {
            quote! {
                fn #handler_helper_name<F>(#(#helper_params),*) -> crate::Result<bytes::Bytes>
                where
                    F: Fn(#(#callback_param_types),*) -> crate::Result<#response_ty>,
                {
                    let response_payload = #response_serialization;

                    Ok(response_payload)
                }
            }
        };
        helper_tokens.push(helper_fn);
    }

    let request_context_ident = if encoding.is_some() || instance_from_request_context {
        Ident::new("request_context", Span::call_site())
    } else {
        Ident::new("_request_context", Span::call_site())
    };

    let helper_call_tokens = if use_service_name_const {
        if encoding.is_some() {
            let mut helper_args: Vec<TokenStream> = vec![
                quote!(payload.as_ref()),
                quote!(&handler),
                quote!(master_node),
                quote!(instance_id),
            ];

            if let Some(arg) = service_instance_call_arg.clone() {
                helper_args.push(arg);
            }

            quote!({
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let master_node = message.master_node().to_string();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            let mut helper_args: Vec<TokenStream> =
                vec![quote!(&handler), quote!(master_node), quote!(instance_id)];

            if let Some(arg) = service_instance_call_arg.clone() {
                helper_args.push(arg);
            }

            quote!({
                let message = #request_context_ident.message();
                let master_node = message.master_node().to_string();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        }
    } else if encoding.is_some() {
        let mut helper_args: Vec<TokenStream> = vec![quote!(payload.as_ref()), quote!(&handler)];

        if instance_from_request_context {
            helper_args.push(quote!(instance_id));
        }

        if let Some(arg) = service_instance_call_arg.clone() {
            helper_args.push(arg);
        }

        if instance_from_request_context {
            quote!({
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            quote!({
                let payload = #request_context_ident.message().payload().as_bytes();
                #handler_helper_name(#(#helper_args),*)
            })
        }
    } else {
        let mut helper_args: Vec<TokenStream> = vec![quote!(&handler)];

        if instance_from_request_context {
            helper_args.push(quote!(instance_id));
        }

        if let Some(arg) = service_instance_call_arg.clone() {
            helper_args.push(arg);
        }

        if instance_from_request_context {
            quote!({
                let message = #request_context_ident.message();
                let payload = message.payload().as_bytes();
                let instance_id = message.instance_id().to_string();
                #handler_helper_name(#(#helper_args),*)
            })
        } else {
            quote!(#handler_helper_name(#(#helper_args),*))
        }
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
                    node_runner.processor().bound_master_node(),
                    node_runner.processor().bound_instance_id(),
                    node_runner.processor().node_name(),
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
                let node_name = node_runner.node_name();
                #service_name_binding

                let mut service = peppylib::ServiceMessenger::listen(
                    node_runner.messenger(),
                    node_runner.master_node(),
                    service_instance_id.as_str(),
                    node_name,
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

    (method, helper_tokens)
}

pub(super) fn build_request_struct_with_name_and_impl(
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

#[allow(clippy::too_many_arguments)]
pub(super) fn build_request_deserializer(
    deserializer_fn_name: &Ident,
    request_spec: &MessageEncodingSpec,
    request_format: &MessageFormat,
    wire_params: &[FunctionParam],
    handler_params: &[FunctionParam],
    label: &str,
    request_struct: Option<&Ident>,
    instance_id_param: Option<&FunctionParam>,
    use_service_name_const: bool,
) -> TokenStream {
    let (context_expr, field_context_expr) = if use_service_name_const {
        (quote!(SERVICE_NAME), quote!(String::from(SERVICE_NAME)))
    } else {
        let context_literal = Literal::string(label);
        (
            quote!(#context_literal),
            quote!(String::from(#context_literal)),
        )
    };

    let reader_type = &request_spec.reader_type;

    let build_deserializer_body =
        |return_ty: TokenStream, result_expr: TokenStream, field_statements: Vec<TokenStream>| {
            let root_stmt = if field_statements.is_empty() {
                quote! {
                    message_reader
                        .get_root::<#reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#context_expr),
                            source,
                        })?;
                }
            } else {
                quote! {
                    let root = message_reader
                        .get_root::<#reader_type>()
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#context_expr),
                            source,
                        })?;
                }
            };
            quote! {
                fn #deserializer_fn_name(payload: &[u8]) -> crate::Result<#return_ty> {
                    let mut cursor = std::io::Cursor::new(payload);
                    let message_reader = capnp::serialize::read_message(
                            &mut cursor,
                            capnp::message::ReaderOptions::new(),
                        )
                        .map_err(|source| crate::Error::CapnpDeserialize {
                            context: String::from(#context_expr),
                            source,
                        })?;

                    #root_stmt

                    #(#field_statements)*

                    Ok(#result_expr)
                }
            }
        };

    if let Some(instance_param) = instance_id_param {
        let instance_ty = &instance_param.ty;
        let request_return_ty = build_return_type_from_params(handler_params, request_struct);
        let return_ty = quote!((#instance_ty, #request_return_ty));

        let schema_lookup = SchemaFieldLookup::new(request_format);
        let mut names = NameGenerator::new();
        let mut field_statements = Vec::new();
        let mut handler_value_map: HashMap<String, Ident> = HashMap::new();
        let mut instance_value_ident = None;
        let instance_field_key = instance_param.ident.to_string();

        for param in wire_params {
            let field_key = param.ident.to_string();
            let (original_name, schema) = schema_lookup.get(&field_key);

            let (mut statements, value_ident) = generate_field_reader_statements(
                &quote!(root),
                original_name.as_str(),
                schema,
                label,
                &field_context_expr,
                &mut names,
            );
            field_statements.append(&mut statements);

            if field_key == instance_field_key {
                instance_value_ident = Some(value_ident);
            } else {
                handler_value_map.insert(field_key, value_ident);
            }
        }

        let instance_value_ident =
            instance_value_ident.expect("instance_id should be present in service requests");

        let ordered_request_values: Vec<Ident> = handler_params
            .iter()
            .map(|param| {
                let key = param.ident.to_string();
                handler_value_map
                    .get(&key)
                    .unwrap_or_else(|| panic!("missing field `{key}` in request payload"))
                    .clone()
            })
            .collect();

        let request_expr =
            build_result_expr_from_values(handler_params, &ordered_request_values, request_struct);
        let result_expr = quote!((#instance_value_ident, #request_expr));

        build_deserializer_body(return_ty, result_expr, field_statements)
    } else {
        let return_ty = build_return_type_from_params(handler_params, request_struct);
        let (field_statements, value_idents) =
            deserialize_fields_from_format(request_format, wire_params, label, &field_context_expr);
        let request_expr =
            build_result_expr_from_values(handler_params, &value_idents, request_struct);

        build_deserializer_body(return_ty, request_expr, field_statements)
    }
}

pub(super) fn build_response_serialization_code(
    response_spec: Option<&ServiceResponseSpec>,
    label: &str,
    callback_call: &TokenStream,
    service_instance_ident: Option<&Ident>,
    use_service_name_const: bool,
) -> TokenStream {
    let Some(spec) = response_spec else {
        return quote!({
            #callback_call?;
            bytes::Bytes::new()
        });
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
    );

    let uses_response_data = spec.format.0.iter().any(|(field_name, _)| {
        !(spec.include_service_instance_id && field_name.as_str() == "instance_id")
    });
    let response_stmt = if uses_response_data {
        quote!(let response = #callback_call?;)
    } else {
        quote!(let _ = #callback_call?;)
    };

    quote!({
        #response_stmt
        #serialization
    })
}

pub(super) fn build_return_type_from_params(
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

pub(super) fn deserialize_fields_from_format(
    request_format: &MessageFormat,
    params: &[FunctionParam],
    label: &str,
    context_expr: &TokenStream,
) -> (Vec<TokenStream>, Vec<Ident>) {
    let schema_lookup = SchemaFieldLookup::new(request_format);
    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut value_idents = Vec::new();

    for param in params {
        let field_key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&field_key);

        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            original_name.as_str(),
            schema,
            label,
            context_expr,
            &mut names,
        );
        field_statements.append(&mut statements);
        value_idents.push(value_ident);
    }

    (field_statements, value_idents)
}

pub(super) fn build_result_expr_from_values(
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

pub(super) fn build_response_payload_tokens(
    spec: &ServiceResponseSpec,
    response_ident: &Ident,
    error_context: &TokenStream,
    service_instance_ident: Option<&Ident>,
) -> TokenStream {
    let builder_type = &spec.builder_type;
    let format = spec.format;
    let builder_ident = Ident::new("response_root", Span::call_site());

    let mut assignments = Vec::new();
    let mut names = NameGenerator::new();

    for (field_name, schema) in &format.0 {
        if spec.include_service_instance_id && field_name == "instance_id" {
            let instance_ident = service_instance_ident
                .expect("service instance identifier should be available when required");
            assignments.push(quote!(#builder_ident.set_instance_id(#instance_ident);));
            continue;
        }

        let field_ident = Ident::new(&sanitize_component(field_name.as_str()), Span::call_site());
        let value_expr = quote!(#response_ident.#field_ident);
        assignments.push(generate_field_assignment(
            &quote!(#builder_ident),
            field_name,
            schema,
            &value_expr,
            &mut names,
        ));
    }

    let init_root_tokens = if assignments.is_empty() {
        quote!(let _ = capnp_msg.init_root::<#builder_type>();)
    } else {
        quote! {
            let mut #builder_ident = capnp_msg.init_root::<#builder_type>();
            #( #assignments )*
        }
    };

    quote!({
        let mut capnp_msg = capnp::message::Builder::new_default();
        {
            #init_root_tokens
        }
        let mut buffer = Vec::new();
        capnp::serialize::write_message(&mut buffer, &capnp_msg).map_err(|source| {
            crate::Error::CapnpSerialize {
                context: #error_context,
                source,
            }
        })?;
        bytes::Bytes::from(buffer)
    })
}
