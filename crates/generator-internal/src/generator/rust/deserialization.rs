use super::identifiers::sanitize_rust_identifier;
use super::serialization::NameGenerator;
use super::type_mapping::primitive_type_token;
use crate::error::{Error, Result};
use crate::generator::naming::{array_item_type_name, sanitize_component, to_camel_case};
use config::node::{ArraySchema, MessageFormat, SchemaType, TypeToken};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

pub fn generate_field_reader_statements(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> Result<(Vec<TokenStream>, Ident)> {
    generate_field_reader_statements_inner(
        reader_expr,
        field_name,
        schema,
        struct_prefix,
        context_expr,
        names,
        true,
    )
}

/// Generates a complete deserialization function:
/// `fn name(payload: &[u8]) -> crate::Result<T> { ... }`.
///
/// `context_expr` must evaluate to `String` at runtime (e.g. `format!(...)`).
pub fn build_deserialize_fn(
    fn_name: &Ident,
    reader_type: &TokenStream,
    context_expr: &TokenStream,
    return_type: &TokenStream,
    field_statements: &[TokenStream],
    result_expr: &TokenStream,
) -> TokenStream {
    let root_stmt = if field_statements.is_empty() {
        quote! {
            message_reader
                .get_root::<#reader_type>()
                .map_err(|source| crate::Error::Deserialization(
                    format!("{}: {}", context, source)
                ))?;
        }
    } else {
        quote! {
            let root = message_reader
                .get_root::<#reader_type>()
                .map_err(|source| crate::Error::Deserialization(
                    format!("{}: {}", context, source)
                ))?;
        }
    };

    quote! {
        fn #fn_name(payload: &[u8]) -> crate::Result<#return_type> {
            #[allow(clippy::all)]
            let context = #context_expr;
            let mut cursor = std::io::Cursor::new(payload);
            let message_reader = capnp::serialize::read_message(
                    &mut cursor,
                    capnp::message::ReaderOptions::new(),
                )
                .map_err(|source| crate::Error::Deserialization(
                    format!("{}: {}", context, source)
                ))?;

            #root_stmt

            #(#field_statements)*

            Ok(#result_expr)
        }
    }
}

/// Deserializes all fields from a `MessageFormat`, iterating directly over format fields.
///
/// Returns `(field_statements, field_inits, value_idents)` where:
/// - `field_statements`: variable binding statements for each deserialized field
/// - `field_inits`: struct field initializers (`field_name: value_ident`)
/// - `value_idents`: the value identifiers for each field
pub fn deserialize_format_fields(
    format: &MessageFormat,
    struct_prefix: &str,
    context_expr: &TokenStream,
) -> Result<(Vec<TokenStream>, Vec<TokenStream>, Vec<Ident>)> {
    let mut names = NameGenerator::new();
    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();
    let mut value_idents = Vec::new();

    for (field_name, schema) in &format.0 {
        let (mut statements, value_ident) = generate_field_reader_statements(
            &quote!(root),
            field_name,
            schema,
            struct_prefix,
            context_expr,
            &mut names,
        )?;
        field_statements.append(&mut statements);
        let field_ident = Ident::new(
            &sanitize_rust_identifier(field_name.as_str()),
            Span::call_site(),
        );
        field_inits.push(quote!(#field_ident: #value_ident));
        value_idents.push(value_ident);
    }

    Ok((field_statements, field_inits, value_idents))
}

fn generate_field_reader_statements_inner(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
    handle_optional: bool,
) -> Result<(Vec<TokenStream>, Ident)> {
    if handle_optional && schema.is_optional() {
        let option_ident = names.next(&format!("{field_name}_opt"));
        if schema_supports_presence_check(schema) {
            let has_method = Ident::new(
                &format!("has_{}", sanitize_component(field_name)),
                Span::call_site(),
            );
            let (inner_statements, value_ident) = generate_field_reader_statements_inner(
                reader_expr,
                field_name,
                schema,
                struct_prefix,
                context_expr,
                names,
                false,
            )?;
            let statements = vec![quote! {
                let #option_ident = if #reader_expr.reborrow().#has_method() {
                    #( #inner_statements )*
                    Some(#value_ident)
                } else {
                    None
                };
            }];
            return Ok((statements, option_ident));
        } else {
            let (mut statements, value_ident) = generate_field_reader_statements_inner(
                reader_expr,
                field_name,
                schema,
                struct_prefix,
                context_expr,
                names,
                false,
            )?;
            statements.push(quote!(let #option_ident = Some(#value_ident);));
            return Ok((statements, option_ident));
        }
    }

    match schema {
        SchemaType::Type(token) => Ok(generate_primitive_reader(
            reader_expr,
            field_name,
            token,
            context_expr,
            names,
        )),
        SchemaType::Primitive(primitive) => Ok(generate_primitive_reader(
            reader_expr,
            field_name,
            &primitive.kind,
            context_expr,
            names,
        )),
        SchemaType::Array(array) => generate_array_reader(
            reader_expr,
            field_name,
            array,
            struct_prefix,
            context_expr,
            names,
        ),
        SchemaType::Object(object) => generate_object_reader(
            reader_expr,
            field_name,
            &object.fields,
            struct_prefix,
            context_expr,
            names,
        ),
    }
}

fn schema_supports_presence_check(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Type(token) => matches!(
            token,
            TypeToken::String | TypeToken::Bytes | TypeToken::Time
        ),
        SchemaType::Primitive(primitive) => matches!(
            primitive.kind,
            TypeToken::String | TypeToken::Bytes | TypeToken::Time
        ),
        SchemaType::Array(_) | SchemaType::Object(_) => true,
    }
}

fn generate_primitive_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    token: &TypeToken,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

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
            let statement = quote! {
                let #value_ident = #reader_expr.reborrow().#method_ident();
            };
            (vec![statement], value_ident)
        }
        TypeToken::String => {
            let reader_ident = names.next("text");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| {
                            #[allow(clippy::all)]
                            let context = #context_expr;
                            crate::Error::Deserialization(
                                format!("field '{}' in {}: {}", #field_literal, context, source)
                            )
                        })?;
                },
                quote! {
                    let #value_ident = #reader_ident
                        .to_str()
                        .map_err(|source| {
                            #[allow(clippy::all)]
                            let context = #context_expr;
                            crate::Error::Deserialization(
                                format!("field '{}' in {}: {}", #field_literal, context, source)
                            )
                        })?
                        .to_owned();
                },
            ];
            (statements, value_ident)
        }
        TypeToken::Bytes => {
            let reader_ident = names.next("data");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| {
                            #[allow(clippy::all)]
                            let context = #context_expr;
                            crate::Error::Deserialization(
                                format!("field '{}' in {}: {}", #field_literal, context, source)
                            )
                        })?;
                },
                quote! {
                    let #value_ident = #reader_ident.to_vec();
                },
            ];
            (statements, value_ident)
        }
        TypeToken::Time => {
            let reader_ident = names.next("timestamp");
            let capnp_ident = names.next("capnp_timestamp");
            let statements = vec![
                quote! {
                    let #reader_ident = #reader_expr
                        .reborrow()
                        .#method_ident()
                        .map_err(|source| {
                            #[allow(clippy::all)]
                            let context = #context_expr;
                            crate::Error::Deserialization(
                                format!("field '{}' in {}: {}", #field_literal, context, source)
                            )
                        })?;
                },
                quote! {
                    let #capnp_ident = peppylib::encoding::CapnpTimestamp {
                        sec: #reader_ident.get_sec(),
                        nsec: #reader_ident.get_nsec(),
                    };
                    let #value_ident = peppylib::encoding::convert_time_from_capnp(#capnp_ident);
                },
            ];
            (statements, value_ident)
        }
    }
}

fn generate_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    array: &ArraySchema,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> Result<(Vec<TokenStream>, Ident)> {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    match array.items.as_ref() {
        SchemaType::Object(object) => generate_object_array_reader(
            reader_expr,
            field_name,
            &object.fields,
            struct_prefix,
            context_expr,
            names,
            array.length,
        ),
        _ => match array.items.as_ref().as_type_token() {
            Some(TypeToken::U8) => Ok(generate_u8_array_reader(
                reader_expr,
                field_name,
                &method_ident,
                array.length,
                context_expr,
                names,
            )),
            Some(token) => Ok(generate_primitive_array_reader(
                reader_expr,
                field_name,
                token,
                &method_ident,
                array.length,
                context_expr,
                names,
            )),
            None => Err(Error::UnsupportedArrayItemSchema {
                field: field_name.to_string(),
            }),
        },
    }
}

fn generate_u8_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    method_ident: &Ident,
    length: Option<usize>,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("data");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| {
                #[allow(clippy::all)]
                let context = #context_expr;
                crate::Error::Deserialization(
                    format!("field '{}' in {}: {}", #field_literal, context, source)
                )
            })?;
    }];

    match length {
        Some(len) => {
            let len_lit = Literal::usize_unsuffixed(len);
            let array_ident = names.next("bytes");
            statements.push(quote! {
                if #reader_ident.len() as usize != #len_lit {
                    let actual = #reader_ident.len() as usize;
                    return Err(crate::Error::Deserialization(
                        format!("invalid fixed bytes length for field '{}': expected {}, got {}", #field_literal, #len_lit, actual)
                    ));
                }
            });
            statements.push(quote! {
                let mut #array_ident = [0u8; #len_lit];
                for (idx, byte) in #reader_ident.iter().enumerate() {
                    #array_ident[idx] = *byte;
                }
                let #value_ident = #array_ident;
            });
        }
        None => {
            statements.push(quote! {
                let #value_ident = #reader_ident.to_vec();
            });
        }
    }

    (statements, value_ident)
}

fn generate_primitive_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    token: &TypeToken,
    method_ident: &Ident,
    length: Option<usize>,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let reader_ident = names.next("list");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

    let element_ty = primitive_type_token(token);

    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| {
                #[allow(clippy::all)]
                let context = #context_expr;
                crate::Error::Deserialization(
                    format!("field '{}' in {}: {}", #field_literal, context, source)
                )
            })?;
    }];

    if let Some(len) = length {
        let len_lit = Literal::usize_unsuffixed(len);
        statements.push(quote! {
            if #reader_ident.len() as usize != #len_lit {
                let actual = #reader_ident.len() as usize;
                return Err(crate::Error::Deserialization(
                    format!("invalid fixed list length for field '{}': expected {}, got {}", #field_literal, #len_lit, actual)
                ));
            }
        });
        statements.push(quote! {
            let mut #value_ident: [#element_ty; #len_lit] = [#element_ty::default(); #len_lit];
            for (idx, element) in #reader_ident.iter().enumerate() {
                #value_ident[idx] = element;
            }
        });
    } else {
        statements.push(quote! {
            let #value_ident = #reader_ident.iter().collect::<Vec<#element_ty>>();
        });
    }

    (statements, value_ident)
}

fn generate_object_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    object: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> Result<(Vec<TokenStream>, Ident)> {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let reader_ident = names.next("reader");
    let field_literal = Literal::string(field_name);
    let mut statements = vec![quote! {
        let #reader_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| {
                #[allow(clippy::all)]
                let context = #context_expr;
                crate::Error::Deserialization(
                    format!("field '{}' in {}: {}", #field_literal, context, source)
                )
            })?;
    }];

    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();
    let nested_prefix = format!("{struct_prefix}{}", to_camel_case(field_name));

    for (nested_name, nested_schema) in object {
        let (mut nested_statements, nested_value_ident) = generate_field_reader_statements(
            &quote!(#reader_ident),
            nested_name.as_str(),
            nested_schema,
            &nested_prefix,
            context_expr,
            names,
        )?;
        field_statements.append(&mut nested_statements);
        let field_ident = Ident::new(&sanitize_rust_identifier(nested_name), Span::call_site());
        field_inits.push(quote!(#field_ident: #nested_value_ident));
    }

    statements.extend(field_statements);

    let struct_name = format!("{struct_prefix}{}", to_camel_case(field_name));
    let struct_ident = Ident::new(&struct_name, Span::call_site());
    let value_ident = names.next(field_name);
    statements.push(quote! {
        let #value_ident = #struct_ident {
            #( #field_inits ),*
        };
    });

    Ok((statements, value_ident))
}

fn generate_object_array_reader(
    reader_expr: &TokenStream,
    field_name: &str,
    fields: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
    length: Option<usize>,
) -> Result<(Vec<TokenStream>, Ident)> {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    let list_ident = names.next("list");
    let result_ident = names.next("result");
    let value_ident = names.next(field_name);
    let field_literal = Literal::string(field_name);

    let nested_prefix = array_item_type_name(struct_prefix, field_name);
    let struct_ident = Ident::new(&nested_prefix, Span::call_site());

    let element_ident = names.next("element");
    let mut field_statements = Vec::new();
    let mut field_inits = Vec::new();

    for (nested_name, nested_schema) in fields {
        let (mut nested_statements, nested_value_ident) = generate_field_reader_statements(
            &quote!(#element_ident),
            nested_name.as_str(),
            nested_schema,
            &nested_prefix,
            context_expr,
            names,
        )?;
        field_statements.append(&mut nested_statements);
        let field_ident = Ident::new(&sanitize_rust_identifier(nested_name), Span::call_site());
        field_inits.push(quote!(#field_ident: #nested_value_ident));
    }

    let mut statements = vec![quote! {
        let #list_ident = #reader_expr
            .reborrow()
            .#method_ident()
            .map_err(|source| {
                #[allow(clippy::all)]
                let context = #context_expr;
                crate::Error::Deserialization(
                    format!("field '{}' in {}: {}", #field_literal, context, source)
                )
            })?;
    }];

    if let Some(len) = length {
        let len_lit = Literal::usize_unsuffixed(len);
        statements.push(quote! {
            let mut #result_ident = Vec::with_capacity(#len_lit);
            for #element_ident in #list_ident.iter() {
                #( #field_statements )*
                #result_ident.push(#struct_ident {
                    #( #field_inits ),*
                });
            }
            let #value_ident: [#struct_ident; #len_lit] = #result_ident.try_into().map_err(|v: Vec<_>| {
                crate::Error::Deserialization(
                    format!("invalid fixed list length for field '{}': expected {}, got {}", #field_literal, #len_lit, v.len())
                )
            })?;
        });
    } else {
        statements.push(quote! {
            let mut #result_ident = Vec::with_capacity(#list_ident.len() as usize);
            for #element_ident in #list_ident.iter() {
                #( #field_statements )*
                #result_ident.push(#struct_ident {
                    #( #field_inits ),*
                });
            }
            let #value_ident = #result_ident;
        });
    }

    Ok((statements, value_ident))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_object_array_reader(length: Option<usize>) -> String {
        let reader_expr = quote!(reader);
        let mut fields = IndexMap::new();
        fields.insert("x".to_string(), SchemaType::Type(TypeToken::I32));
        let context_expr = quote!("TestMsg");
        let mut names = NameGenerator::new();

        let (statements, _) = generate_object_array_reader(
            &reader_expr,
            "frames",
            &fields,
            "Test",
            &context_expr,
            &mut names,
            length,
        )
        .unwrap();

        let combined = quote! { #( #statements )* };
        combined.to_string()
    }

    #[test]
    fn object_array_reader_fixed_length_uses_try_into() {
        let code = call_object_array_reader(Some(4));
        assert!(
            code.contains("try_into"),
            "fixed-length path must use try_into, got: {code}"
        );
        assert!(
            code.contains("Vec :: with_capacity (4"),
            "expected fixed literal 4 in Vec::with_capacity, got: {code}"
        );
        assert!(
            code.contains(r#""frames" , 4 , v . len ()"#),
            "error message must reference field name and expected length, got: {code}"
        );
    }

    #[test]
    fn object_array_reader_dynamic_length_uses_vec() {
        let code = call_object_array_reader(None);
        assert!(
            !code.contains("try_into"),
            "dynamic-length path must not use try_into, got: {code}"
        );
        assert!(
            code.contains("Vec :: with_capacity"),
            "dynamic-length path must use Vec::with_capacity, got: {code}"
        );
    }
}
