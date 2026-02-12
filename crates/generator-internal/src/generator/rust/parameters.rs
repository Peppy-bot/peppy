use super::type_mapping::render_tokens;
use crate::error::{Error, Result};
use crate::generator::naming::{sanitize_rust_identifier, to_camel_case};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

/// Generates Rust struct code from node parameters configuration.
///
/// This function takes a `NodeArguments` map (where values are type specifications like "string", "u16", etc.
/// or nested objects) and generates Rust struct definitions with proper typing.
///
/// Each top-level parameter field gets its own module to namespace nested types.
/// Always generates a `Parameters` struct, even if empty.
///
/// Returns an error if any field name contains invalid characters.
pub fn generate_parameters_struct(parameters: &config::NodeArguments) -> Result<String> {
    use config::AnyType;

    validate_parameter_field_names(parameters)?;

    let mut main_fields = Vec::new();
    let mut modules = Vec::new();

    for (field_name, type_spec) in parameters {
        let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());
        let module_name = sanitize_rust_identifier(field_name);
        let module_ident = Ident::new(&module_name, Span::call_site());

        match type_spec {
            AnyType::Object(_) => {
                let struct_name = to_camel_case(field_name);
                let struct_ident = Ident::new(&struct_name, Span::call_site());
                let module_contents = generate_parameter_module_contents(type_spec, &struct_name);

                modules.push(quote! {
                    pub mod #module_ident {
                        #module_contents
                    }
                });

                main_fields.push(quote!(pub #field_ident: #module_ident::#struct_ident));
            }
            AnyType::String(type_name) => {
                let rust_type = type_name_to_rust_type(type_name);
                main_fields.push(quote!(pub #field_ident: #rust_type));
            }
            _ => {}
        }
    }

    let main_struct = quote! {
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        pub struct Parameters {
            #( #main_fields ),*
        }
    };

    let mut all_tokens = main_struct;
    for module in modules {
        all_tokens = quote! { #all_tokens #module };
    }

    Ok(render_tokens(all_tokens))
}

fn validate_parameter_field_names(parameters: &config::NodeArguments) -> Result<()> {
    use config::AnyType;
    use config::consts::ALLOWED_CONFIG_CHARS;

    fn is_valid_field_name(name: &str, allowed: &str) -> bool {
        !name.is_empty() && name.chars().all(|c| allowed.contains(c))
    }

    fn validate_fields(
        fields: &std::collections::BTreeMap<String, AnyType>,
        allowed: &'static str,
    ) -> Result<()> {
        for (field_name, field_spec) in fields {
            if !is_valid_field_name(field_name, allowed) {
                return Err(Error::InvalidParameterFieldName {
                    name: field_name.clone(),
                    allowed,
                });
            }
            if let AnyType::Object(nested_fields) = field_spec {
                validate_fields(nested_fields, allowed)?;
            }
        }
        Ok(())
    }

    for (field_name, type_spec) in parameters {
        if !is_valid_field_name(field_name, ALLOWED_CONFIG_CHARS) {
            return Err(Error::InvalidParameterFieldName {
                name: field_name.clone(),
                allowed: ALLOWED_CONFIG_CHARS,
            });
        }
        if let AnyType::Object(fields) = type_spec {
            validate_fields(fields, ALLOWED_CONFIG_CHARS)?;
        }
    }

    Ok(())
}

fn generate_parameter_module_contents(
    type_spec: &config::AnyType,
    struct_name: &str,
) -> TokenStream {
    let mut structs = Vec::new();
    generate_parameter_struct(type_spec, struct_name, &mut structs);

    let tokens: TokenStream = structs.into_iter().collect();
    tokens
}

fn generate_parameter_struct(
    type_spec: &config::AnyType,
    struct_name: &str,
    structs: &mut Vec<TokenStream>,
) {
    use config::AnyType;

    if let AnyType::Object(fields) = type_spec {
        let struct_ident = Ident::new(struct_name, Span::call_site());
        let mut field_tokens = Vec::new();

        for (field_name, field_spec) in fields {
            let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());

            match field_spec {
                AnyType::String(type_name) => {
                    let rust_type = type_name_to_rust_type(type_name);
                    field_tokens.push(quote!(pub #field_ident: #rust_type));
                }
                AnyType::Object(_) => {
                    let nested_struct_name = nested_struct_name(struct_name, field_name);
                    let nested_struct_ident = Ident::new(&nested_struct_name, Span::call_site());

                    generate_parameter_struct(field_spec, &nested_struct_name, structs);

                    field_tokens.push(quote!(pub #field_ident: #nested_struct_ident));
                }
                _ => {}
            }
        }

        structs.push(quote! {
            #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
            pub struct #struct_ident {
                #( #field_tokens ),*
            }
        });
    }
}

fn nested_struct_name(parent_struct: &str, field_name: &str) -> String {
    format!("{parent_struct}{}", to_camel_case(field_name))
}

fn type_name_to_rust_type(type_name: &str) -> TokenStream {
    match type_name {
        "bool" => quote!(bool),
        "string" | "str" => quote!(String),
        "bytes" => quote!(Vec<u8>),
        "time" => quote!(std::time::SystemTime),
        "u8" => quote!(u8),
        "u16" => quote!(u16),
        "u32" => quote!(u32),
        "u64" => quote!(u64),
        "i8" => quote!(i8),
        "i16" => quote!(i16),
        "i32" => quote!(i32),
        "i64" => quote!(i64),
        "f32" | "float" => quote!(f32),
        "f64" | "double" => quote!(f64),
        _ => quote!(String),
    }
}
