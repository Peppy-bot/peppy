use super::identifiers::sanitize_rust_identifier;
use super::type_mapping::{primitive_type_token, render_tokens};
use crate::error::{Error, Result};
use crate::generator::naming::to_camel_case;
use config::ParameterSpec;
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use std::collections::BTreeMap;

/// Generates Rust struct code from node parameters configuration.
///
/// Each top-level parameter group gets its own module to namespace nested types.
/// Always generates a `Parameters` struct, even if empty.
///
/// Returns an error if any field name contains characters outside
/// [`config::consts::ALLOWED_CONFIG_CHARS`].
pub fn generate_parameters_struct(parameters: &config::ParameterSchema) -> Result<String> {
    validate_parameter_schema(parameters)?;

    let mut main_fields = Vec::new();
    let mut modules = Vec::new();

    for (field_name, spec) in parameters {
        let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());

        match spec {
            ParameterSpec::Group(_) => {
                let module_ident =
                    Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());
                let struct_name = to_camel_case(field_name);
                let struct_ident = Ident::new(&struct_name, Span::call_site());
                let module_contents =
                    generate_parameter_module_contents(spec, &struct_name, field_name)?;

                modules.push(quote! {
                    pub mod #module_ident {
                        #module_contents
                    }
                });

                main_fields.push(quote!(pub #field_ident: #module_ident::#struct_ident));
            }
            ParameterSpec::Primitive { kind, .. } => {
                let rust_type = primitive_type_token(kind);
                main_fields.push(quote!(pub #field_ident: #rust_type));
            }
            ParameterSpec::Array { .. } => {
                return Err(Error::UnsupportedArrayParameter {
                    path: field_name.clone(),
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

/// Validates that parameter field names contain only allowed characters.
///
/// Type validity is already enforced at parse time by [`config::ParameterSpec`],
/// so this only checks identifier-name rules. Shared with the Python generator.
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
        fields: &BTreeMap<String, ParameterSpec>,
        parent_path: &str,
        allowed: &'static str,
    ) -> Result<()> {
        for (field_name, spec) in fields {
            if !is_valid_field_name(field_name, allowed) {
                return Err(Error::InvalidParameterFieldName {
                    name: field_name.clone(),
                    allowed,
                });
            }
            if let ParameterSpec::Group(nested) = spec {
                let field_path = join_path(parent_path, field_name);
                validate_fields(nested, &field_path, allowed)?;
            } else if let ParameterSpec::Array { .. } = spec {
                return Err(Error::UnsupportedArrayParameter {
                    path: join_path(parent_path, field_name),
                });
            }
        }
        Ok(())
    }

    validate_fields(parameters, "", ALLOWED_CONFIG_CHARS)
}

fn generate_parameter_module_contents(
    spec: &ParameterSpec,
    struct_name: &str,
    root_path: &str,
) -> Result<TokenStream> {
    let mut structs = Vec::new();
    generate_parameter_struct(spec, struct_name, root_path, &mut structs)?;

    let tokens: TokenStream = structs.into_iter().collect();
    Ok(tokens)
}

fn generate_parameter_struct(
    spec: &ParameterSpec,
    struct_name: &str,
    current_path: &str,
    structs: &mut Vec<TokenStream>,
) -> Result<()> {
    let ParameterSpec::Group(fields) = spec else {
        // Only Groups produce nested structs. Primitives are inlined as field types.
        return Err(Error::InvariantViolation {
            context: format!("expected parameter group at `{current_path}`"),
        });
    };

    let struct_ident = Ident::new(struct_name, Span::call_site());
    let mut field_tokens = Vec::new();

    for (field_name, field_spec) in fields {
        let field_ident = Ident::new(&sanitize_rust_identifier(field_name), Span::call_site());
        let field_path = format!("{current_path}.{field_name}");

        match field_spec {
            ParameterSpec::Primitive { kind, .. } => {
                let rust_type = primitive_type_token(kind);
                field_tokens.push(quote!(pub #field_ident: #rust_type));
            }
            ParameterSpec::Group(_) => {
                let nested_struct_name = nested_struct_name(struct_name, field_name);
                let nested_struct_ident = Ident::new(&nested_struct_name, Span::call_site());

                generate_parameter_struct(field_spec, &nested_struct_name, &field_path, structs)?;

                field_tokens.push(quote!(pub #field_ident: #nested_struct_ident));
            }
            ParameterSpec::Array { .. } => {
                return Err(Error::UnsupportedArrayParameter { path: field_path });
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
