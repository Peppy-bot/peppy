use super::type_mapping::{sanitize_capnp_field_name, schema_type_to_tokens};
use crate::error::{Error, Result};
use crate::generator::naming::sanitize_rust_identifier;
use crate::generator::types::validate_message_format_field_names;
use config::encoding::{CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper};
use config::node::{MessageFormat, SchemaType};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;

#[derive(Default)]
pub struct GenerationContext {
    structs: Vec<StructDefinition>,
    private_items: Vec<TokenStream>,
}

impl GenerationContext {
    pub fn add_struct(&mut self, ident: Ident, fields: Vec<(Ident, TokenStream)>) {
        if let Some(existing) = self.structs.iter_mut().find(|def| def.ident == ident) {
            *existing = StructDefinition { ident, fields };
        } else {
            self.structs.push(StructDefinition { ident, fields });
        }
    }

    pub fn add_private_struct(&mut self, tokens: TokenStream) {
        self.private_items.push(tokens);
    }

    pub fn wrap_optional_type(&mut self, ty: TokenStream) -> TokenStream {
        quote!(Option<#ty>)
    }

    pub fn into_tokens(self) -> Vec<TokenStream> {
        let mut items: Vec<TokenStream> = Vec::new();
        items.extend(self.structs.into_iter().map(StructDefinition::into_tokens));
        items.extend(self.private_items);
        items
    }
}

struct StructDefinition {
    ident: Ident,
    fields: Vec<(Ident, TokenStream)>,
}

impl StructDefinition {
    fn into_tokens(self) -> TokenStream {
        let ident = self.ident;
        let field_tokens: Vec<TokenStream> = self
            .fields
            .into_iter()
            .map(|(field_ident, ty)| {
                let name = field_ident;
                let field_ty = ty;
                quote!(pub #name: #field_ty)
            })
            .collect();

        if field_tokens.is_empty() {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct #ident {}
            }
        } else {
            quote! {
                #[derive(Debug, Clone)]
                #[allow(dead_code)]
                pub struct #ident {
                    #( #field_tokens ),*
                }
            }
        }
    }
}

pub fn map_message_format(format: Option<&MessageFormat>) -> Result<Option<CapnpSchemaArtifacts>> {
    match format {
        Some(format) => {
            validate_message_format_field_names(format, "message_format")?;
            MessageFormatMapper::new(format.clone())
                .map_message_format_to_capnpn()
                .map(Some)
                .map_err(Error::MessageEncoding)
        }
        None => Ok(None),
    }
}

pub struct SchemaFieldLookup<'a> {
    entries: HashMap<String, (&'a String, &'a SchemaType)>,
}

impl<'a> SchemaFieldLookup<'a> {
    pub fn new(format: &'a MessageFormat) -> Self {
        let mut entries = HashMap::with_capacity(format.0.len() * 2);
        for (name, schema) in &format.0 {
            let capnp_key = sanitize_capnp_field_name(name);
            entries.insert(capnp_key, (name, schema));

            let rust_key = sanitize_rust_identifier(name);
            entries.entry(rust_key).or_insert((name, schema));
        }
        Self { entries }
    }

    pub fn get(&self, key: &str) -> (&'a String, &'a SchemaType) {
        *self
            .entries
            .get(key)
            .unwrap_or_else(|| panic!("missing schema entry for field `{key}`"))
    }
}

pub fn collect_function_params(
    accept_format_artifacts: Option<&CapnpSchemaArtifacts>,
    return_format_artifacts: Option<&CapnpSchemaArtifacts>,
    struct_prefix: &str,
    context: &mut GenerationContext,
    response_struct_name_override: Option<&Ident>,
) -> Result<Vec<FunctionParam>> {
    if let Some(return_artifacts) = return_format_artifacts {
        let response_struct_name = response_struct_name_override
            .map(|ident| ident.to_string())
            .unwrap_or_else(|| format!("{struct_prefix}Response"));
        let response_ident = Ident::new(&response_struct_name, Span::call_site());
        let mut fields = Vec::new();
        let mut ctor_params: Vec<TokenStream> = Vec::new();
        let mut ctor_bindings: Vec<TokenStream> = Vec::new();

        for (field_name, schema) in &return_artifacts.message_format().0 {
            let field_ident = Ident::new(
                &sanitize_rust_identifier(field_name.as_str()),
                Span::call_site(),
            );
            let field_ty =
                schema_type_to_tokens(schema, &response_struct_name, field_name, context);
            let ctor_ident = field_ident.clone();
            let ctor_ty = field_ty.clone();
            ctor_params.push(quote!(#ctor_ident: #ctor_ty));
            ctor_bindings.push(quote!(#ctor_ident));
            fields.push((field_ident, field_ty));
        }

        context.add_struct(response_ident.clone(), fields);

        let ctor_tokens = if ctor_params.is_empty() {
            quote! {
                impl #response_ident {
                    pub fn new() -> Self {
                        Self {}
                    }
                }
            }
        } else {
            quote! {
                impl #response_ident {
                    pub fn new(#(#ctor_params),*) -> Self {
                        Self {
                            #( #ctor_bindings ),*
                        }
                    }
                }
            }
        };
        context.add_private_struct(ctor_tokens);
    }

    let Some(artifacts) = accept_format_artifacts else {
        return Ok(Vec::new());
    };

    let format = artifacts.message_format();
    let schema_lookup = SchemaFieldLookup::new(format);
    let capnp_params = artifacts
        .build_function_params()
        .map_err(Error::MessageEncoding)?;

    let mut params = Vec::with_capacity(capnp_params.len());
    for param in capnp_params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&key);

        let ident = Ident::new(&sanitize_rust_identifier(original_name), Span::call_site());
        let ty = schema_type_to_tokens(schema, struct_prefix, original_name, context);

        params.push(FunctionParam::new(ident, ty));
    }

    Ok(params)
}
