use super::context::GenerationContext;
use crate::generator::naming::{sanitize_rust_identifier, to_camel_case};
use config::encoding::FunctionParam;
use config::node::{SchemaType, TypeToken};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use syn::{File, parse2};

pub fn schema_type_to_tokens(
    schema: &SchemaType,
    struct_prefix: &str,
    field_name: &str,
    context: &mut GenerationContext,
) -> TokenStream {
    let ty = match schema {
        SchemaType::Type(token) => primitive_type_token(token),
        SchemaType::Primitive(primitive) => primitive_type_token(&primitive.kind),
        SchemaType::Array(array) => {
            let item_ty = match array.items.as_ref().as_type_token() {
                Some(token) => primitive_type_token(token),
                None => panic!("unsupported nested schema type in array `{field_name}`"),
            };

            if let Some(length) = array.length {
                let len_lit = Literal::usize_unsuffixed(length);
                quote!([#item_ty; #len_lit])
            } else {
                quote!(Vec<#item_ty>)
            }
        }
        SchemaType::Object(object) => {
            let struct_name = format!("{struct_prefix}{}", to_camel_case(field_name));
            let struct_ident = Ident::new(&struct_name, Span::call_site());

            let mut fields = Vec::with_capacity(object.fields.len());
            for (nested_name, nested_schema) in &object.fields {
                let field_ident = Ident::new(
                    &sanitize_rust_identifier(nested_name.as_str()),
                    Span::call_site(),
                );
                let field_ty =
                    schema_type_to_tokens(nested_schema, &struct_name, nested_name, context);
                fields.push((field_ident, field_ty));
            }

            context.add_struct(struct_ident.clone(), fields);
            quote!(#struct_ident)
        }
    };

    if schema.is_optional() {
        context.wrap_optional_type(ty)
    } else {
        ty
    }
}

pub fn primitive_type_token(token: &TypeToken) -> TokenStream {
    match token {
        TypeToken::Bool => quote!(bool),
        TypeToken::String => quote!(String),
        TypeToken::Bytes => quote!(Vec<u8>),
        TypeToken::Time => quote!(std::time::SystemTime),
        TypeToken::U8 => quote!(u8),
        TypeToken::U16 => quote!(u16),
        TypeToken::U32 => quote!(u32),
        TypeToken::U64 => quote!(u64),
        TypeToken::I8 => quote!(i8),
        TypeToken::I16 => quote!(i16),
        TypeToken::I32 => quote!(i32),
        TypeToken::I64 => quote!(i64),
        TypeToken::F32 => quote!(f32),
        TypeToken::F64 => quote!(f64),
    }
}

pub fn sanitize_capnp_field_name(input: &str) -> String {
    fn to_pascal_case(input: &str) -> String {
        let mut result = String::new();

        for segment in input
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
        {
            let mut chars = segment.chars();
            if let Some(first) = chars.next() {
                result.push(first.to_ascii_uppercase());
                for ch in chars {
                    result.push(ch.to_ascii_lowercase());
                }
            }
        }

        result
    }

    let mut output = to_pascal_case(input);
    if output.is_empty() {
        output.push_str("Field");
    }

    let mut chars = output.chars();
    let mut camel = String::with_capacity(output.len());
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    if camel.is_empty() {
        "_field".to_string()
    } else {
        camel
    }
}

pub fn unused_params_stmt(params: &[FunctionParam]) -> TokenStream {
    if params.is_empty() {
        TokenStream::new()
    } else if params.len() == 1 {
        let ident = &params[0].ident;
        quote! {
            let _ = &#ident;
        }
    } else {
        let refs: Vec<TokenStream> = params
            .iter()
            .map(|param| {
                let ident = &param.ident;
                quote!(&#ident)
            })
            .collect();
        quote! {
            let _ = (#(#refs),*);
        }
    }
}

pub fn render_tokens(tokens: TokenStream) -> String {
    parse2::<File>(tokens.clone())
        .map(|file| prettyplease::unparse(&file))
        .unwrap_or_else(|_| tokens.to_string())
}
