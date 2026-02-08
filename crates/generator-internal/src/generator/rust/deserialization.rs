use super::serialization::NameGenerator;
use super::type_mapping::primitive_type_token;
use crate::generator::naming::{sanitize_component, to_camel_case};
use config::node::{ArraySchema, SchemaType, TypeToken};
use indexmap::IndexMap;
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;

pub(super) fn generate_field_reader_statements(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
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

fn generate_field_reader_statements_inner(
    reader_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    struct_prefix: &str,
    context_expr: &TokenStream,
    names: &mut NameGenerator,
    handle_optional: bool,
) -> (Vec<TokenStream>, Ident) {
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
            );
            let statements = vec![quote! {
                let #option_ident = if #reader_expr.reborrow().#has_method() {
                    #( #inner_statements )*
                    Some(#value_ident)
                } else {
                    None
                };
            }];
            return (statements, option_ident);
        } else {
            let (mut statements, value_ident) = generate_field_reader_statements_inner(
                reader_expr,
                field_name,
                schema,
                struct_prefix,
                context_expr,
                names,
                false,
            );
            statements.push(quote!(let #option_ident = Some(#value_ident);));
            return (statements, option_ident);
        }
    }

    match schema {
        SchemaType::Type(token) => {
            generate_primitive_reader(reader_expr, field_name, token, context_expr, names)
        }
        SchemaType::Primitive(primitive) => generate_primitive_reader(
            reader_expr,
            field_name,
            &primitive.kind,
            context_expr,
            names,
        ),
        SchemaType::Array(array) => {
            generate_array_reader(reader_expr, field_name, array, context_expr, names)
        }
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
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: #context_expr,
                            source,
                        })?;
                },
                quote! {
                    let #value_ident = #reader_ident
                        .to_str()
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: #context_expr,
                            source: capnp::Error::failed(source.to_string()),
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
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: #context_expr,
                            source,
                        })?;
                },
                quote! {
                    let len = #reader_ident.len();
                    let mut #value_ident = Vec::with_capacity(len as usize);
                    for byte in #reader_ident.iter() {
                        #value_ident.push(*byte);
                    }
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
                        .map_err(|source| crate::Error::CapnpField {
                            field: String::from(#field_literal),
                            context: #context_expr,
                            source,
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
    context_expr: &TokenStream,
    names: &mut NameGenerator,
) -> (Vec<TokenStream>, Ident) {
    let method_ident = Ident::new(
        &format!("get_{}", sanitize_component(field_name)),
        Span::call_site(),
    );
    match array.items.as_ref().as_type_token() {
        Some(TypeToken::U8) => generate_u8_array_reader(
            reader_expr,
            field_name,
            &method_ident,
            array.length,
            context_expr,
            names,
        ),
        Some(token) => generate_primitive_array_reader(
            reader_expr,
            field_name,
            token,
            &method_ident,
            array.length,
            context_expr,
            names,
        ),
        None => panic!("unsupported nested schema type in array `{field_name}`"),
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
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
                source,
            })?;
    }];

    match length {
        Some(len) => {
            let len_lit = Literal::usize_unsuffixed(len);
            let array_ident = names.next("bytes");
            statements.push(quote! {
                if #reader_ident.len() as usize != #len_lit {
                    let actual = #reader_ident.len() as usize;
                    return Err(crate::Error::InvalidFixedBytes {
                        field: String::from(#field_literal),
                        expected: #len_lit,
                        actual,
                    });
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
            let vec_ident = names.next("bytes");
            statements.push(quote! {
                let mut #vec_ident = Vec::with_capacity(#reader_ident.len() as usize);
                for byte in #reader_ident.iter() {
                    #vec_ident.push(*byte);
                }
                let #value_ident = #vec_ident;
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
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
                source,
            })?;
    }];

    if let Some(len) = length {
        let len_lit = Literal::usize_unsuffixed(len);
        statements.push(quote! {
            if #reader_ident.len() as usize != #len_lit {
                let actual = #reader_ident.len() as usize;
                return Err(crate::Error::InvalidFixedListLength {
                    field: String::from(#field_literal),
                    expected: #len_lit,
                    actual,
                });
            }
        });
        statements.push(quote! {
            let mut #value_ident: [#element_ty; #len_lit] = [#element_ty::default(); #len_lit];
            for (idx, element) in #reader_ident.iter().enumerate() {
                #value_ident[idx] = element;
            }
        });
    } else {
        let vec_ident = names.next("values");
        statements.push(quote! {
            let mut #vec_ident = Vec::with_capacity(#reader_ident.len() as usize);
            for value in #reader_ident.iter() {
                #vec_ident.push(value);
            }
            let #value_ident = #vec_ident;
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
) -> (Vec<TokenStream>, Ident) {
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
            .map_err(|source| crate::Error::CapnpField {
                field: String::from(#field_literal),
                context: #context_expr,
                source,
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
        );
        field_statements.append(&mut nested_statements);
        let field_ident = Ident::new(&sanitize_component(nested_name), Span::call_site());
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

    (statements, value_ident)
}
