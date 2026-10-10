use super::identifiers::sanitize_rust_identifier;
use super::type_mapping::schema_type_to_tokens;
use crate::error::{Error, Result};
use crate::generator::naming::sanitize_capnp_field_name;
use crate::generator::types::{
    NormalizedFieldNames, validate_fixed_length_array_items,
    validate_generated_type_name_collisions, validate_message_format_field_names,
};
use config::node::{MessageFormat, PeppygenLanguage, SchemaType};
use encoding::{CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper};
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

    /// Registers a struct like [`Self::add_struct`], plus a `new`
    /// constructor that takes every field in order.
    pub fn add_struct_with_constructor(&mut self, ident: Ident, fields: Vec<(Ident, TokenStream)>) {
        let ctor_params: Vec<TokenStream> = fields
            .iter()
            .map(|(field_ident, ty)| quote!(#field_ident: #ty))
            .collect();
        let ctor_bindings: Vec<&Ident> =
            fields.iter().map(|(field_ident, _)| field_ident).collect();
        self.private_items.push(quote! {
            impl #ident {
                pub fn new(#(#ctor_params),*) -> Self {
                    Self {
                        #( #ctor_bindings ),*
                    }
                }
            }
        });
        self.add_struct(ident, fields);
    }

    pub fn add_private_struct(&mut self, tokens: TokenStream) {
        self.private_items.push(tokens);
    }

    /// Registers a struct with standard metadata fields: `instance_id`, `core_node`,
    /// and an optional `data` field of the given type.
    pub fn add_metadata_struct(&mut self, struct_ident: Ident, data_ident: Option<&Ident>) {
        let mut fields = vec![
            (Ident::new("instance_id", Span::call_site()), quote!(String)),
            (Ident::new("core_node", Span::call_site()), quote!(String)),
        ];
        if let Some(data_ident) = data_ident {
            fields.push((Ident::new("data", Span::call_site()), quote!(#data_ident)));
        }
        self.add_struct(struct_ident, fields);
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
            .map(|(field_ident, ty)| quote!(pub #field_ident: #ty))
            .collect();

        quote! {
            #[derive(Debug, Clone)]
            #[allow(dead_code)]
            pub struct #ident {
                #( #field_tokens ),*
            }
        }
    }
}

pub fn map_message_format(
    schema_name: &str,
    format: Option<&MessageFormat>,
) -> Result<Option<CapnpSchemaArtifacts>> {
    match format {
        Some(format) => {
            validate_normalized_field_names_for_rust(format)?;
            validate_message_format_field_names(format, "message_format")?;
            validate_fixed_length_array_items(format)?;
            validate_generated_type_name_collisions(format, "Message")?;
            MessageFormatMapper::new(schema_name, format.clone())
                .map_message_format_to_capnpn()
                .map(Some)
                .map_err(Error::MessageEncoding)
        }
        None => Ok(None),
    }
}

fn validate_normalized_field_names_for_rust(format: &MessageFormat) -> Result<()> {
    validate_object_field_name_collisions_for_rust(format.0.iter(), "message_format")
}

fn validate_object_field_name_collisions_for_rust<'a, I>(fields: I, context: &str) -> Result<()>
where
    I: IntoIterator<Item = (&'a String, &'a SchemaType)>,
{
    let mut rust_identifiers =
        NormalizedFieldNames::new(PeppygenLanguage::Rust, "rust identifier", context);
    let mut capnp_fields =
        NormalizedFieldNames::new(PeppygenLanguage::Rust, "capnp field name", context);

    for (field_name, schema) in fields {
        rust_identifiers.claim(sanitize_rust_identifier(field_name), field_name)?;
        capnp_fields.claim(sanitize_capnp_field_name(field_name), field_name)?;
        validate_nested_field_name_collisions_for_rust(schema, &format!("{context}.{field_name}"))?;
    }

    Ok(())
}

fn validate_nested_field_name_collisions_for_rust(schema: &SchemaType, path: &str) -> Result<()> {
    match schema {
        SchemaType::Object(object) => {
            validate_object_field_name_collisions_for_rust(object.fields.iter(), path)
        }
        SchemaType::Array(array) => validate_nested_field_name_collisions_for_rust(
            array.items.as_ref(),
            &format!("{path}[]"),
        ),
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

pub struct SchemaFieldLookup<'a> {
    entries: HashMap<String, (&'a String, &'a SchemaType)>,
}

impl<'a> SchemaFieldLookup<'a> {
    pub fn new(format: &'a MessageFormat) -> Result<Self> {
        let mut entries = HashMap::with_capacity(format.0.len() * 2);
        for (name, schema) in &format.0 {
            let capnp_key = sanitize_capnp_field_name(name);
            insert_lookup_entry(&mut entries, capnp_key, name, schema, "capnp field name")?;

            let rust_key = sanitize_rust_identifier(name);
            insert_lookup_entry(&mut entries, rust_key, name, schema, "rust identifier")?;
        }
        Ok(Self { entries })
    }

    pub fn get(&self, key: &str) -> Result<(&'a String, &'a SchemaType)> {
        self.entries
            .get(key)
            .copied()
            .ok_or_else(|| Error::InvariantViolation {
                context: format!("missing schema entry for field `{key}`"),
            })
    }
}

fn insert_lookup_entry<'a>(
    entries: &mut HashMap<String, (&'a String, &'a SchemaType)>,
    key: String,
    field_name: &'a String,
    schema: &'a SchemaType,
    normalization: &'static str,
) -> Result<()> {
    if let Some((existing_name, _)) = entries.get(&key) {
        if *existing_name != field_name {
            return Err(Error::FieldNameNormalizationCollision {
                language: PeppygenLanguage::Rust,
                context: String::from("message_format"),
                normalization,
                normalized: key,
                first_field: (*existing_name).clone(),
                second_field: field_name.clone(),
            });
        }
        return Ok(());
    }

    entries.insert(key, (field_name, schema));
    Ok(())
}

/// Registers the `response_ident` struct of a response format, with a `new`
/// constructor that takes every field. Its nested structs take the
/// response struct's own name as their prefix.
pub fn add_response_struct(
    context: &mut GenerationContext,
    artifacts: &CapnpSchemaArtifacts,
    response_ident: &Ident,
) -> Result<()> {
    let response_struct_name = response_ident.to_string();
    let mut fields = Vec::new();

    for (field_name, schema) in &artifacts.message_format().0 {
        let field_ident = Ident::new(
            &sanitize_rust_identifier(field_name.as_str()),
            Span::call_site(),
        );
        let field_ty = schema_type_to_tokens(schema, &response_struct_name, field_name, context)?;
        fields.push((field_ident, field_ty));
    }

    context.add_struct_with_constructor(response_ident.clone(), fields);
    Ok(())
}

/// The function parameters of a message format, one per field, with the
/// structs nested in it registered in `context` under `struct_prefix`. An
/// absent format has no parameters.
pub fn collect_function_params(
    format_artifacts: Option<&CapnpSchemaArtifacts>,
    struct_prefix: &str,
    context: &mut GenerationContext,
) -> Result<Vec<FunctionParam>> {
    let Some(artifacts) = format_artifacts else {
        return Ok(Vec::new());
    };

    let format = artifacts.message_format();
    let schema_lookup = SchemaFieldLookup::new(format)?;
    let capnp_params = artifacts
        .build_function_params()
        .map_err(Error::MessageEncoding)?;

    let mut params = Vec::with_capacity(capnp_params.len());
    for param in capnp_params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup.get(&key)?;

        let ident = Ident::new(&sanitize_rust_identifier(original_name), Span::call_site());
        let ty = schema_type_to_tokens(schema, struct_prefix, original_name, context)?;

        params.push(FunctionParam::new(ident, ty));
    }

    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `$optional` on a scalar never reaches code generation: it is rejected
    /// when the config is parsed, identically for every language.
    #[test]
    fn reject_optional_scalar_at_parse_time() {
        let result: std::result::Result<MessageFormat, _> = serde_json5::from_str(
            r#"
            {
                maybe_code: {
                    $type: "u32",
                    $optional: true
                }
            }
            "#,
        );
        assert!(
            result.is_err(),
            "optional on a scalar should be rejected at parse time"
        );
    }

    #[test]
    fn allow_optional_pointer_types() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                maybe_text: {
                    $type: "string",
                    $optional: true
                },
                maybe_payload: {
                    $type: "bytes",
                    $optional: true
                }
            }
            "#,
        )
        .expect("optional string/bytes remain supported");

        map_message_format("test", Some(&format)).expect("optional pointer types generate");
    }

    #[rstest::rstest]
    #[case::flat(
        r#"
        {
            "foo-bar": "u8",
            foo_bar: "u8"
        }
        "#,
        "message_format"
    )]
    #[case::nested(
        r#"
        {
            status: {
                $type: "object",
                "foo-bar": "u8",
                foo_bar: "u8"
            }
        }
        "#,
        "message_format.status"
    )]
    fn reject_colliding_normalized_field_names_for_rust(
        #[case] format: &str,
        #[case] expected_context: &str,
    ) {
        let format: MessageFormat = serde_json5::from_str(format).unwrap();

        let err = match map_message_format("test", Some(&format)) {
            Ok(_) => panic!("expected FieldNameNormalizationCollision"),
            Err(err) => err,
        };
        match err {
            Error::FieldNameNormalizationCollision {
                language,
                context,
                normalization,
                normalized,
                first_field,
                second_field,
            } => {
                assert_eq!(language, PeppygenLanguage::Rust);
                assert_eq!(context, expected_context);
                assert_eq!(normalization, "rust identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }
}
