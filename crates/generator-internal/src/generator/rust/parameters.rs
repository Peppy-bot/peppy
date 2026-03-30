use super::identifiers::sanitize_rust_identifier;
use super::type_mapping::render_tokens;
use crate::error::{Error, Result};
use crate::generator::naming::to_camel_case;
use config::AnyType;
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use std::collections::BTreeMap;

/// Generates Rust struct code from node parameters configuration.
///
/// This function takes a `ParameterSchema` map (where values are type specifications like "string", "u16", etc.
/// or nested objects) and generates Rust struct definitions with proper typing.
///
/// Each top-level parameter field gets its own module to namespace nested types.
/// Always generates a `Parameters` struct, even if empty.
///
/// Returns an error if any field name contains invalid characters.
pub fn generate_parameters_struct(parameters: &config::ParameterSchema) -> Result<String> {
    validate_parameter_schema(parameters)?;

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
                let module_contents =
                    generate_parameter_module_contents(type_spec, &struct_name, field_name)?;

                modules.push(quote! {
                    pub mod #module_ident {
                        #module_contents
                    }
                });

                main_fields.push(quote!(pub #field_ident: #module_ident::#struct_ident));
            }
            AnyType::String(type_name) => {
                let rust_type = type_name_to_rust_type(type_name, field_name)?;
                main_fields.push(quote!(pub #field_ident: #rust_type));
            }
            _ => {
                return Err(Error::UnsupportedParameterSpecType {
                    path: field_name.clone(),
                    kind: type_spec.type_name(),
                });
            }
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

/// Validates that parameter specs only contain supported field names and schema shapes.
pub fn validate_parameter_schema(parameters: &config::ParameterSchema) -> Result<()> {
    use config::consts::ALLOWED_CONFIG_CHARS;

    fn is_valid_field_name(name: &str, allowed: &str) -> bool {
        !name.is_empty() && name.chars().all(|c| allowed.contains(c))
    }

    fn join_path(parent_path: &str, field_name: &str) -> String {
        if parent_path.is_empty() {
            field_name.to_string()
        } else {
            format!("{parent_path}.{field_name}")
        }
    }

    fn validate_fields(
        fields: &BTreeMap<String, AnyType>,
        parent_path: &str,
        allowed: &'static str,
    ) -> Result<()> {
        for (field_name, field_spec) in fields {
            if !is_valid_field_name(field_name, allowed) {
                return Err(Error::InvalidParameterFieldName {
                    name: field_name.clone(),
                    allowed,
                });
            }
            let field_path = join_path(parent_path, field_name);
            match field_spec {
                AnyType::String(type_name) => {
                    let _ = type_name_to_rust_type(type_name, &field_path)?;
                }
                AnyType::Object(nested_fields) => {
                    validate_fields(nested_fields, &field_path, allowed)?;
                }
                _ => {
                    return Err(Error::UnsupportedParameterSpecType {
                        path: field_path,
                        kind: field_spec.type_name(),
                    });
                }
            }
        }
        Ok(())
    }

    validate_fields(parameters, "", ALLOWED_CONFIG_CHARS)
}

fn generate_parameter_module_contents(
    type_spec: &config::AnyType,
    struct_name: &str,
    root_path: &str,
) -> Result<TokenStream> {
    let mut structs = Vec::new();
    generate_parameter_struct(type_spec, struct_name, root_path, &mut structs)?;

    let tokens: TokenStream = structs.into_iter().collect();
    Ok(tokens)
}

fn generate_parameter_struct(
    type_spec: &config::AnyType,
    struct_name: &str,
    current_path: &str,
    structs: &mut Vec<TokenStream>,
) -> Result<()> {
    let AnyType::Object(fields) = type_spec else {
        return Err(Error::UnsupportedParameterSpecType {
            path: current_path.to_string(),
            kind: type_spec.type_name(),
        });
    };

    let struct_ident = Ident::new(struct_name, Span::call_site());
    let mut field_tokens = Vec::new();

    for (field_name, field_spec) in fields {
        let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());
        let field_path = format!("{current_path}.{field_name}");

        match field_spec {
            AnyType::String(type_name) => {
                let rust_type = type_name_to_rust_type(type_name, &field_path)?;
                field_tokens.push(quote!(pub #field_ident: #rust_type));
            }
            AnyType::Object(_) => {
                let nested_struct_name = nested_struct_name(struct_name, field_name);
                let nested_struct_ident = Ident::new(&nested_struct_name, Span::call_site());

                generate_parameter_struct(field_spec, &nested_struct_name, &field_path, structs)?;

                field_tokens.push(quote!(pub #field_ident: #nested_struct_ident));
            }
            _ => {
                return Err(Error::UnsupportedParameterSpecType {
                    path: field_path,
                    kind: field_spec.type_name(),
                });
            }
        }
    }

    structs.push(quote! {
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
        pub struct #struct_ident {
            #( #field_tokens ),*
        }
    });

    Ok(())
}

fn nested_struct_name(parent_struct: &str, field_name: &str) -> String {
    format!("{parent_struct}{}", to_camel_case(field_name))
}

fn type_name_to_rust_type(type_name: &str, path: &str) -> Result<TokenStream> {
    match type_name {
        "bool" => Ok(quote!(bool)),
        "string" | "str" => Ok(quote!(String)),
        "bytes" => Ok(quote!(Vec<u8>)),
        "time" => Ok(quote!(std::time::SystemTime)),
        "u8" => Ok(quote!(u8)),
        "u16" => Ok(quote!(u16)),
        "u32" => Ok(quote!(u32)),
        "u64" => Ok(quote!(u64)),
        "i8" => Ok(quote!(i8)),
        "i16" => Ok(quote!(i16)),
        "i32" => Ok(quote!(i32)),
        "i64" => Ok(quote!(i64)),
        "f32" | "float" => Ok(quote!(f32)),
        "f64" | "double" => Ok(quote!(f64)),
        _ => Err(Error::UnsupportedParameterTypeName {
            path: path.to_string(),
            type_name: type_name.to_string(),
        }),
    }
}
