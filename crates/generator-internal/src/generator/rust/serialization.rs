use super::identifiers::sanitize_rust_identifier;
use crate::generator::naming::sanitize_component;
use crate::{error::Error, error::Result};
use config::encoding::FunctionParam;
use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;

pub struct MessageEncodingSpec {
    pub builder_type: TokenStream,
    pub assignments: Vec<TokenStream>,
    pub reader_type: TokenStream,
}

#[derive(Default)]
pub struct NameGenerator {
    counter: usize,
}

impl NameGenerator {
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    pub fn next(&mut self, hint: &str) -> Ident {
        let sanitized = sanitize_rust_identifier(hint);
        let suffix = self.counter;
        self.counter += 1;
        let base = if sanitized.is_empty() {
            "tmp".to_string()
        } else {
            sanitized
        };
        Ident::new(&format!("{base}_{suffix}"), Span::call_site())
    }
}

pub fn generate_assignments_for_format(
    builder_ident: &Ident,
    format: &MessageFormat,
    params: &[FunctionParam],
) -> Result<Vec<TokenStream>> {
    let mut param_lookup: HashMap<String, Ident> = HashMap::new();
    for param in params {
        param_lookup.insert(param.ident.to_string(), param.ident.clone());
    }

    let mut assignments = Vec::with_capacity(format.0.len());
    let mut name_gen = NameGenerator::new();
    let builder_expr = quote!(#builder_ident);

    for (field_name, schema) in &format.0 {
        let sanitized = sanitize_rust_identifier(field_name);
        let param_ident =
            param_lookup
                .get(&sanitized)
                .cloned()
                .ok_or_else(|| Error::InvariantViolation {
                    context: format!("missing parameter for field `{field_name}`"),
                })?;
        let value_expr = quote!(#param_ident);
        assignments.push(generate_field_assignment(
            &builder_expr,
            field_name,
            schema,
            &value_expr,
            &mut name_gen,
        )?);
    }

    Ok(assignments)
}

pub fn generate_assignments_from_struct(
    builder_ident: &Ident,
    format: &MessageFormat,
    struct_ident: &Ident,
) -> Result<Vec<TokenStream>> {
    let mut assignments = Vec::with_capacity(format.0.len());
    let mut name_gen = NameGenerator::new();
    let builder_expr = quote!(#builder_ident);

    for (field_name, schema) in &format.0 {
        let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());
        let value_expr = quote!(#struct_ident.#field_ident);
        assignments.push(generate_field_assignment(
            &builder_expr,
            field_name,
            schema,
            &value_expr,
            &mut name_gen,
        )?);
    }

    Ok(assignments)
}

pub fn generate_field_assignment(
    builder_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> Result<TokenStream> {
    generate_field_assignment_inner(
        builder_expr,
        field_name,
        schema,
        value_expr,
        names,
        true,
        false,
    )
}

fn generate_field_assignment_inner(
    builder_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
    handle_optional: bool,
    value_is_ref: bool,
) -> Result<TokenStream> {
    if handle_optional && schema.is_optional() {
        match schema {
            SchemaType::Object(_) => {
                let binding = names.next("value");
                let inner_expr = quote!(#binding);
                let inner = generate_field_assignment_inner(
                    builder_expr,
                    field_name,
                    schema,
                    &inner_expr,
                    names,
                    false,
                    false,
                )?;
                return Ok(quote! {
                    if let Some(#binding) = (#value_expr).cloned() {
                        #inner
                    }
                });
            }
            _ => {
                let binding = names.next("value");
                let inner_expr = quote!(#binding);
                let inner = generate_field_assignment_inner(
                    builder_expr,
                    field_name,
                    schema,
                    &inner_expr,
                    names,
                    false,
                    true,
                )?;
                return Ok(quote! {
                    if let Some(#binding) = (#value_expr).as_ref() {
                        #inner
                    }
                });
            }
        }
    }

    let method_component = sanitize_component(field_name);
    let set_method = Ident::new(&format!("set_{method_component}"), Span::call_site());
    let init_method = Ident::new(&format!("init_{method_component}"), Span::call_site());

    match schema {
        SchemaType::Type(token) => Ok(primitive_field_assignment(
            builder_expr,
            &set_method,
            &init_method,
            value_expr,
            names,
            value_is_ref,
            token,
        )),
        SchemaType::Primitive(primitive) => Ok(primitive_field_assignment(
            builder_expr,
            &set_method,
            &init_method,
            value_expr,
            names,
            value_is_ref,
            &primitive.kind,
        )),
        SchemaType::Array(array) => {
            let item_token = array.items.as_ref().as_type_token();
            if matches!(item_token, Some(TypeToken::U8)) {
                Ok(quote!(#builder_expr.#set_method(#value_expr.as_ref());))
            } else if let Some(token) = item_token {
                generate_list_assignment(
                    builder_expr,
                    &init_method,
                    value_expr,
                    field_name,
                    array.length,
                    token,
                    names,
                )
            } else {
                Err(Error::UnsupportedArrayItemSchema {
                    field: field_name.to_string(),
                })
            }
        }
        SchemaType::Object(object) => generate_object_assignment(
            builder_expr,
            &init_method,
            value_expr,
            &object.fields,
            names,
        ),
    }
}

fn primitive_field_assignment(
    builder_expr: &TokenStream,
    set_method: &Ident,
    init_method: &Ident,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
    value_is_ref: bool,
    token: &TypeToken,
) -> TokenStream {
    match token {
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => {
            let value_tokens = if value_is_ref {
                quote!(*#value_expr)
            } else {
                quote!(#value_expr)
            };
            quote!(#builder_expr.#set_method(#value_tokens);)
        }
        TypeToken::String => {
            quote!(#builder_expr.#set_method(#value_expr.as_str());)
        }
        TypeToken::Bytes => {
            quote!(#builder_expr.#set_method(#value_expr.as_ref());)
        }
        TypeToken::Time => {
            let time_expr = if value_is_ref {
                quote!(*#value_expr)
            } else {
                quote!(#value_expr)
            };
            generate_time_assignment(builder_expr, init_method, &time_expr, names)
        }
    }
}

fn generate_time_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> TokenStream {
    let timestamp_ident = names.next("timestamp");
    let builder_ident = names.next("timestamp_builder");

    quote! {
        let #timestamp_ident = peppylib::encoding::convert_time(#value_expr);
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #builder_ident.set_sec(#timestamp_ident.sec);
        #builder_ident.set_nsec(#timestamp_ident.nsec);
    }
}

fn generate_list_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    field_name: &str,
    length: Option<usize>,
    token: &TypeToken,
    names: &mut NameGenerator,
) -> Result<TokenStream> {
    let list_ident = names.next("list");
    let idx_ident = names.next("idx");
    let element_ident = names.next("value");

    let length_expr = match length {
        Some(len) => {
            let len_lit = Literal::u32_unsuffixed(u32::try_from(len).map_err(|_| {
                Error::InvariantViolation {
                    context: format!("list length {len} for field `{field_name}` exceeds u32::MAX"),
                }
            })?);
            quote!(#len_lit)
        }
        None => quote!(u32::try_from((#value_expr).len()).expect("list length exceeds u32::MAX")),
    };

    let element_setter = match token {
        TypeToken::String => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_str());),
        TypeToken::Bytes => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_ref());),
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => quote!(#list_ident.set(#idx_ident as u32, *#element_ident);),
        TypeToken::Time => {
            return Err(Error::InvariantViolation {
                context: format!("time arrays are not supported for field `{field_name}`"),
            });
        }
    };

    Ok(quote! {
        let mut #list_ident = #builder_expr.reborrow().#init_method(#length_expr);
        for (#idx_ident, #element_ident) in (#value_expr).iter().enumerate() {
            #element_setter
        }
    })
}

fn generate_object_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    fields: &IndexMap<String, SchemaType>,
    names: &mut NameGenerator,
) -> Result<TokenStream> {
    let builder_ident = names.next("builder");
    let mut nested = Vec::with_capacity(fields.len());

    for (nested_name, nested_schema) in fields {
        let nested_ident = Ident::new(
            &sanitize_rust_identifier(nested_name.as_str()),
            Span::call_site(),
        );
        let nested_value_expr = quote!(#value_expr.#nested_ident);
        nested.push(generate_field_assignment(
            &quote!(#builder_ident),
            nested_name,
            nested_schema,
            &nested_value_expr,
            names,
        )?);
    }

    Ok(quote! {
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #(#nested)*
    })
}

/// Generates a block expression that serializes fields into a Cap'n Proto message
/// and returns `bytes::Bytes`.
///
/// `pre_statements` are emitted before `init_root` (e.g. struct field unpacking).
/// `error_context` must evaluate to `String` at runtime.
pub fn build_serialize_payload(
    builder_type: &TokenStream,
    pre_statements: &[TokenStream],
    assignments: &[TokenStream],
    error_context: &TokenStream,
) -> TokenStream {
    let init_root_tokens = if assignments.is_empty() {
        quote!(let _ = capnp_msg.init_root::<#builder_type>();)
    } else {
        quote! {
            let mut root = capnp_msg.init_root::<#builder_type>();
            #(#assignments)*
        }
    };

    quote!({
        let mut capnp_msg = capnp::message::Builder::new_default();
        {
            #(#pre_statements)*
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
