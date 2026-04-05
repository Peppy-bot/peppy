use super::context::GenerationContext;
use super::identifiers::sanitize_rust_identifier;
use crate::error::{Error, Result};
use crate::generator::naming::{array_item_field_name, to_camel_case};
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
) -> Result<TokenStream> {
    let ty = match schema {
        SchemaType::Type(token) => primitive_type_token(token),
        SchemaType::Primitive(primitive) => primitive_type_token(&primitive.kind),
        SchemaType::Array(array) => {
            let item_ty = match array.items.as_ref() {
                SchemaType::Object(_) => {
                    let item_field = array_item_field_name(field_name);
                    schema_type_to_tokens(
                        array.items.as_ref(),
                        struct_prefix,
                        &item_field,
                        context,
                    )?
                }
                other => {
                    let token =
                        other
                            .as_type_token()
                            .ok_or_else(|| Error::UnsupportedArrayItemSchema {
                                field: field_name.to_string(),
                            })?;
                    primitive_type_token(token)
                }
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
                    schema_type_to_tokens(nested_schema, &struct_name, nested_name, context)?;
                fields.push((field_ident, field_ty));
            }

            context.add_struct(struct_ident.clone(), fields);
            quote!(#struct_ident)
        }
    };

    let ty = if schema.is_optional() {
        context.wrap_optional_type(ty)
    } else {
        ty
    };
    Ok(ty)
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
