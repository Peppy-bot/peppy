use super::identifiers::sanitize_rust_identifier;
use super::type_mapping::schema_type_to_tokens;
use crate::error::{Error, Result};
use crate::generator::naming::sanitize_capnp_field_name;
use crate::generator::types::{
    is_scalar_copy_type, type_token_name, validate_fixed_length_array_items,
    validate_generated_type_name_collisions, validate_message_format_field_names,
};
use config::encoding::{CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper};
use config::node::{MessageFormat, PeppygenLanguage, SchemaType};
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
            validate_optional_scalar_fields_for_rust(format)?;
            MessageFormatMapper::new(schema_name, format.clone())
                .map_message_format_to_capnpn()
                .map(Some)
                .map_err(Error::MessageEncoding)
        }
        None => Ok(None),
    }
}

fn validate_optional_scalar_schema_for_rust(schema: &SchemaType, path: &str) -> Result<()> {
    match schema {
        SchemaType::Primitive(primitive) => {
            if primitive.optional && is_scalar_copy_type(&primitive.kind) {
                return Err(Error::UnsupportedOptionalScalarType {
                    language: PeppygenLanguage::Rust,
                    field: path.to_string(),
                    item: type_token_name(&primitive.kind),
                });
            }
            Ok(())
        }
        SchemaType::Array(array) => {
            validate_optional_scalar_schema_for_rust(array.items.as_ref(), &format!("{path}[]"))
        }
        SchemaType::Object(object) => {
            for (field_name, nested) in &object.fields {
                let nested_path = format!("{path}.{field_name}");
                validate_optional_scalar_schema_for_rust(nested, &nested_path)?;
            }
            Ok(())
        }
        SchemaType::Type(_) => Ok(()),
    }
}

fn validate_optional_scalar_fields_for_rust(format: &MessageFormat) -> Result<()> {
    for (field_name, schema) in &format.0 {
        validate_optional_scalar_schema_for_rust(schema, field_name)?;
    }
    Ok(())
}

fn validate_normalized_field_names_for_rust(format: &MessageFormat) -> Result<()> {
    validate_object_field_name_collisions_for_rust(format.0.iter(), "message_format")
}

fn validate_object_field_name_collisions_for_rust<'a, I>(fields: I, context: &str) -> Result<()>
where
    I: IntoIterator<Item = (&'a String, &'a SchemaType)>,
{
    let mut rust_identifiers: HashMap<String, String> = HashMap::new();
    let mut capnp_fields: HashMap<String, String> = HashMap::new();

    for (field_name, schema) in fields {
        let rust_ident = sanitize_rust_identifier(field_name);
        if let Some(previous_field) = rust_identifiers.get(&rust_ident) {
            if previous_field != field_name {
                return Err(Error::FieldNameNormalizationCollision {
                    language: PeppygenLanguage::Rust,
                    context: context.to_string(),
                    normalization: "rust identifier",
                    normalized: rust_ident,
                    first_field: previous_field.clone(),
                    second_field: field_name.clone(),
                });
            }
        } else {
            rust_identifiers.insert(rust_ident, field_name.clone());
        }

        let capnp_field = sanitize_capnp_field_name(field_name);
        if let Some(previous_field) = capnp_fields.get(&capnp_field) {
            if previous_field != field_name {
                return Err(Error::FieldNameNormalizationCollision {
                    language: PeppygenLanguage::Rust,
                    context: context.to_string(),
                    normalization: "capnp field name",
                    normalized: capnp_field,
                    first_field: previous_field.clone(),
                    second_field: field_name.clone(),
                });
            }
        } else {
            capnp_fields.insert(capnp_field, field_name.clone());
        }

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
                schema_type_to_tokens(schema, &response_struct_name, field_name, context)?;
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

    #[test]
    fn reject_optional_scalar_for_rust() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                maybe_code: {
                    $type: "u32",
                    $optional: true
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_optional_scalar_fields_for_rust(&format).unwrap_err();
        match err {
            Error::UnsupportedOptionalScalarType {
                language,
                field,
                item,
            } => {
                assert_eq!(language, PeppygenLanguage::Rust);
                assert_eq!(field, "maybe_code");
                assert_eq!(item, "u32");
            }
            other => panic!("expected UnsupportedOptionalScalarType, got: {other:?}"),
        }
    }

    #[test]
    fn reject_optional_nested_scalar_at_parse_time() {
        // `$optional` on nested object fields is rejected at deserialization
        // time by the ObjectSchema visitor, not at generator validation time.
        let result: std::result::Result<MessageFormat, _> = serde_json5::from_str(
            r#"
            {
                status: {
                    $type: "object",
                    healthy: {
                        $type: "bool",
                        $optional: true
                    }
                }
            }
            "#,
        );
        assert!(
            result.is_err(),
            "optional on nested object field should be rejected at parse time"
        );
    }

    #[test]
    fn allow_optional_pointer_types_for_rust() {
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
        .unwrap();

        validate_optional_scalar_fields_for_rust(&format)
            .expect("optional string/bytes should remain supported for Rust");
    }

    #[test]
    fn reject_colliding_normalized_field_names_for_rust() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                "foo-bar": "u8",
                foo_bar: "u8"
            }
            "#,
        )
        .unwrap();

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
                assert_eq!(context, "message_format");
                assert_eq!(normalization, "rust identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }

    #[test]
    fn reject_nested_colliding_normalized_field_names_for_rust() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                status: {
                    $type: "object",
                    "foo-bar": "u8",
                    foo_bar: "u8"
                }
            }
            "#,
        )
        .unwrap();

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
                assert_eq!(context, "message_format.status");
                assert_eq!(normalization, "rust identifier");
                assert_eq!(normalized, "foo_bar");
                assert_eq!(first_field, "foo-bar");
                assert_eq!(second_field, "foo_bar");
            }
            other => panic!("expected FieldNameNormalizationCollision, got: {other:?}"),
        }
    }
}
