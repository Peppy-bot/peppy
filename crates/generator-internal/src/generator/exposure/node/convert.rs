//! Code generation for the bridges between canonical exposure JSON and the
//! peppygen typed message structs.
//!
//! The value rules mirror the canonical schema mapping in
//! `exposure::json_schema`: `time` is RFC 3339, `bytes` and `u8` arrays are
//! base64, `u64` and `i64` are decimal strings, floats must be finite. The
//! generated code delegates every conversion to `peppy_mcp_runtime::bridge`
//! so the rules live in one tested place; this module only wires fields to
//! helpers, following the struct naming the Rust backend uses
//! (`{prefix}{CamelCase(field)}` for nested objects, `_item` for array
//! items).

use crate::generator::naming::{array_item_field_name, to_camel_case};
use crate::generator::rust::identifiers::sanitize_rust_identifier;
use config::node::{MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::{format_ident, quote};

/// An expression of type `serde_json::Value` (using `?` with `String`
/// errors) rendering `source`, a struct value with `fields`.
pub(super) fn object_to_json_expr(
    fields: &IndexMap<String, SchemaType>,
    source: TokenStream,
    depth: usize,
) -> TokenStream {
    let nested = format_ident!("nested_{depth}");
    let object = format_ident!("object_{depth}");
    let statements: Vec<TokenStream> = fields
        .iter()
        .map(|(name, schema)| {
            let name_literal = Literal::string(name);
            let field_ident = field_ident(name);
            let field_source = quote!(#nested.#field_ident);
            if schema.is_optional() {
                let converted = value_to_json_expr(schema, quote!(value), depth + 1);
                quote! {
                    if let Some(value) = #field_source {
                        #object.insert(#name_literal.to_string(), #converted);
                    }
                }
            } else {
                let converted = value_to_json_expr(schema, field_source, depth + 1);
                quote! {
                    #object.insert(#name_literal.to_string(), #converted);
                }
            }
        })
        .collect();
    quote!({
        let #nested = #source;
        let mut #object = serde_json::Map::new();
        #(#statements)*
        serde_json::Value::Object(#object)
    })
}

/// Same as [`object_to_json_expr`] for a whole message format.
pub(super) fn format_to_json_expr(format: &MessageFormat, source: TokenStream) -> TokenStream {
    object_to_json_expr(&format.0, source, 0)
}

/// An expression of type `serde_json::Value` rendering one non-optional
/// value of `schema` (optionality is handled by the caller).
fn value_to_json_expr(schema: &SchemaType, source: TokenStream, depth: usize) -> TokenStream {
    match schema {
        SchemaType::Type(_) | SchemaType::Primitive(_) => {
            let token = schema
                .as_type_token()
                .expect("primitive schemas carry a type token");
            scalar_to_json_expr(token, source)
        }
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Object(object) => {
                let items = format_ident!("items_{depth}");
                let item_expr = object_to_json_expr(&object.fields, quote!(item), depth + 1);
                quote!({
                    let mut #items = Vec::new();
                    for item in #source {
                        #items.push(#item_expr);
                    }
                    serde_json::Value::Array(#items)
                })
            }
            items => {
                let token = items
                    .as_type_token()
                    .expect("array items are scalar or object after validation");
                if *token == TypeToken::U8 {
                    // `u8` arrays share the `bytes` rendering.
                    quote!(serde_json::Value::String(
                        peppy_mcp_runtime::bridge::bytes_to_base64(&#source)
                    ))
                } else {
                    let items_ident = format_ident!("items_{depth}");
                    let item_expr = scalar_to_json_expr(token, quote!(item));
                    quote!({
                        let mut #items_ident = Vec::new();
                        for item in #source {
                            #items_ident.push(#item_expr);
                        }
                        serde_json::Value::Array(#items_ident)
                    })
                }
            }
        },
        SchemaType::Object(object) => object_to_json_expr(&object.fields, source, depth),
    }
}

fn scalar_to_json_expr(token: &TypeToken, source: TokenStream) -> TokenStream {
    match token {
        TypeToken::Bool
        | TypeToken::String
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32 => quote!(serde_json::Value::from(#source)),
        TypeToken::U64 | TypeToken::I64 => {
            quote!(serde_json::Value::String(#source.to_string()))
        }
        TypeToken::F32 => {
            quote!(peppy_mcp_runtime::bridge::float_to_json(f64::from(#source))?)
        }
        TypeToken::F64 => quote!(peppy_mcp_runtime::bridge::float_to_json(#source)?),
        TypeToken::Time => quote!(serde_json::Value::String(
            peppy_mcp_runtime::bridge::time_to_rfc3339(#source)
        )),
        TypeToken::Bytes => quote!(serde_json::Value::String(
            peppy_mcp_runtime::bridge::bytes_to_base64(&#source)
        )),
    }
}

/// An expression (using `?` with `String` errors) constructing the struct
/// `module::struct_name` from `value`, a `&serde_json::Value` object.
/// `child_prefix` names nested structs the way the Rust backend does.
pub(super) fn object_from_json_expr(
    fields: &IndexMap<String, SchemaType>,
    value: TokenStream,
    module: &TokenStream,
    struct_name: &str,
    child_prefix: &str,
    depth: usize,
) -> TokenStream {
    let nested = format_ident!("input_{depth}");
    let struct_ident = Ident::new(struct_name, Span::call_site());
    let field_inits: Vec<TokenStream> = fields
        .iter()
        .map(|(name, schema)| {
            let name_literal = Literal::string(name);
            let field = field_ident(name);
            if schema.is_optional() {
                let from_value = value_from_json_expr(
                    schema,
                    quote!(value),
                    name,
                    module,
                    child_prefix,
                    depth + 1,
                );
                quote! {
                    #field: match peppy_mcp_runtime::bridge::optional(#nested, #name_literal) {
                        Some(value) => Some(#from_value),
                        None => None,
                    }
                }
            } else {
                let from_value = value_from_json_expr(
                    schema,
                    quote!(peppy_mcp_runtime::bridge::require(#nested, #name_literal)?),
                    name,
                    module,
                    child_prefix,
                    depth + 1,
                );
                quote!(#field: #from_value)
            }
        })
        .collect();
    quote!({
        let #nested = #value;
        #module::#struct_ident {
            #(#field_inits),*
        }
    })
}

/// An expression converting `value`, a `&serde_json::Value`, into one
/// non-optional value of `schema`.
fn value_from_json_expr(
    schema: &SchemaType,
    value: TokenStream,
    name: &str,
    module: &TokenStream,
    prefix: &str,
    depth: usize,
) -> TokenStream {
    let name_literal = Literal::string(name);
    match schema {
        SchemaType::Type(_) | SchemaType::Primitive(_) => {
            let token = schema
                .as_type_token()
                .expect("primitive schemas carry a type token");
            scalar_from_json_expr(token, value, &name_literal)
        }
        SchemaType::Array(array) => {
            let item_expr_for = |item_schema: &SchemaType| match item_schema {
                SchemaType::Object(object) => {
                    let item_struct = nested_struct_name(prefix, &array_item_field_name(name));
                    object_from_json_expr(
                        &object.fields,
                        quote!(item),
                        module,
                        &item_struct,
                        &item_struct,
                        depth + 1,
                    )
                }
                items => {
                    let token = items
                        .as_type_token()
                        .expect("array items are scalar or object after validation");
                    scalar_from_json_expr(token, quote!(item), &name_literal)
                }
            };
            let is_u8 = array.items.as_ref().as_type_token() == Some(&TypeToken::U8);
            match (is_u8, array.length) {
                (true, None) => {
                    quote!(peppy_mcp_runtime::bridge::value_bytes(#value, #name_literal)?)
                }
                (true, Some(length)) => {
                    let length_literal = Literal::usize_unsuffixed(length);
                    quote!({
                        let bytes = peppy_mcp_runtime::bridge::value_bytes(#value, #name_literal)?;
                        <[u8; #length_literal]>::try_from(bytes).map_err(|_| {
                            format!(
                                "`{}` must decode to exactly {} bytes",
                                #name_literal, #length_literal
                            )
                        })?
                    })
                }
                (false, length) => {
                    let out = format_ident!("out_{depth}");
                    let item_expr = item_expr_for(array.items.as_ref());
                    let finish = match length {
                        None => quote!(#out),
                        Some(length) => {
                            let length_literal = Literal::usize_unsuffixed(length);
                            quote! {
                                #out.try_into().map_err(|_| {
                                    format!(
                                        "`{}` must have exactly {} items",
                                        #name_literal, #length_literal
                                    )
                                })?
                            }
                        }
                    };
                    quote!({
                        let items = peppy_mcp_runtime::bridge::value_array(#value, #name_literal)?;
                        let mut #out = Vec::with_capacity(items.len());
                        for item in items {
                            #out.push(#item_expr);
                        }
                        #finish
                    })
                }
            }
        }
        SchemaType::Object(object) => {
            let struct_name = nested_struct_name(prefix, name);
            object_from_json_expr(
                &object.fields,
                value,
                module,
                &struct_name,
                &struct_name,
                depth,
            )
        }
    }
}

fn scalar_from_json_expr(
    token: &TypeToken,
    value: TokenStream,
    name_literal: &Literal,
) -> TokenStream {
    let helper = |ident: &str| {
        let helper_ident = Ident::new(ident, Span::call_site());
        quote!(peppy_mcp_runtime::bridge::#helper_ident(#value, #name_literal)?)
    };
    match token {
        TypeToken::Bool => helper("value_bool"),
        TypeToken::String => helper("value_string"),
        TypeToken::Bytes => helper("value_bytes"),
        TypeToken::Time => helper("value_time"),
        TypeToken::U8 => helper("value_u8"),
        TypeToken::U16 => helper("value_u16"),
        TypeToken::U32 => helper("value_u32"),
        TypeToken::U64 => helper("value_u64_decimal"),
        TypeToken::I8 => helper("value_i8"),
        TypeToken::I16 => helper("value_i16"),
        TypeToken::I32 => helper("value_i32"),
        TypeToken::I64 => helper("value_i64_decimal"),
        TypeToken::F32 => {
            let inner = helper("value_f64");
            quote!((#inner as f32))
        }
        TypeToken::F64 => helper("value_f64"),
    }
}

/// The struct name the Rust backend gives a nested object field.
fn nested_struct_name(prefix: &str, field_name: &str) -> String {
    format!("{prefix}{}", to_camel_case(field_name))
}

/// The Rust identifier the backend gives a message field.
pub(super) fn field_ident(name: &str) -> Ident {
    Ident::new(&sanitize_rust_identifier(name), Span::call_site())
}
