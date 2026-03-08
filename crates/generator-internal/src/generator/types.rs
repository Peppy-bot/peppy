use crate::error::{Error, Result};
use crate::generator::common::CrateDeployMode;
use config::consts::PeppyDirs;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, PeppygenLanguage, PrimitiveSchema,
    SchemaType, SubscribedAction, SubscribedService, SubscribedTopic, TypeToken,
};
use indexmap::IndexMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    ExposedTopic,
    ExposedService,
    ExposedAction,
    SubscribedTopic,
    SubscribedService,
    SubscribedAction,
}

#[derive(Debug, Clone)]
pub struct SubscribedActionMessage {
    pub goal_request: Option<MessageFormat>,
    pub goal_response: Option<MessageFormat>,
    pub feedback: Option<MessageFormat>,
    pub result_request: Option<MessageFormat>,
    pub result_response: Option<MessageFormat>,
}

/// Describes a concrete subscriber/exposer interface that a deployment requires.
#[derive(Debug, Clone)]
pub enum InterfaceVariant {
    ExposedTopic(ExposedTopic),
    ExposedService(ExposedService),
    ExposedAction(ExposedAction),
    SubscribedTopic(SubscribedTopic, MessageFormat),
    SubscribedService(SubscribedService, MessageFormat, MessageFormat),
    SubscribedAction(SubscribedAction, SubscribedActionMessage),
}

/// Maps a deployment interface to the message format required to bind it.
#[derive(Debug, Clone)]
pub struct DeploymentInterface {
    interface: InterfaceVariant,
}

impl DeploymentInterface {
    pub fn new(interface: InterfaceVariant) -> Self {
        Self { interface }
    }

    pub fn interface(&self) -> &InterfaceVariant {
        &self.interface
    }

    pub fn into_interface(self) -> InterfaceVariant {
        self.interface
    }
}

pub struct InterfaceArtifact {
    pub node_name: String,
    pub kind: InterfaceKind,
    pub interface: Option<InterfaceVariant>,
    pub code_output: String,
    pub submodule: Option<String>,
}

impl InterfaceArtifact {
    pub fn from_kind(node_name: &str, kind: InterfaceKind, code_output: String) -> Self {
        Self::from_kind_with_submodule(node_name, kind, code_output, None)
    }

    pub fn from_kind_with_submodule(
        node_name: &str,
        kind: InterfaceKind,
        code_output: String,
        submodule: Option<String>,
    ) -> Self {
        Self {
            node_name: node_name.to_string(),
            kind,
            interface: None,
            code_output,
            submodule,
        }
    }
}

/// Collects deployment interfaces and produces generated artifacts when finalized.
pub trait LanguageGenerator {
    fn push_section(&mut self, section: InterfaceArtifact);
    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()>;
    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()>;
    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()>;
    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: MessageFormat,
    ) -> Result<()>;
    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
    ) -> Result<()>;
    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: &SubscribedActionMessage,
    ) -> Result<()>;
    /// Finalizes the builder and return a path to the library
    fn build(
        self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &PeppyDirs,
        deploy_mode: CrateDeployMode,
    ) -> Result<()>;
}

impl DeploymentInterface {
    pub fn register_with<B: LanguageGenerator + ?Sized>(&self, backend: &mut B) -> Result<()> {
        match self.interface() {
            InterfaceVariant::ExposedTopic(topic) => backend.add_exposed_topic(topic),
            InterfaceVariant::ExposedService(service) => backend.add_exposed_service(service),
            InterfaceVariant::ExposedAction(action) => backend.add_exposed_action(action),
            InterfaceVariant::SubscribedTopic(topic, format) => {
                backend.add_subscribed_topic(topic, format.clone())
            }
            InterfaceVariant::SubscribedService(service, request_arguments, response_arguments) => {
                backend.add_subscribed_service(service, request_arguments, response_arguments)
            }
            InterfaceVariant::SubscribedAction(action, messages) => {
                backend.add_subscribed_action(action, messages)
            }
        }
    }
}

/// Filters out empty `MessageFormat`s, returning `None` for formats with no fields.
pub fn non_empty_message_format(format: Option<&MessageFormat>) -> Option<&MessageFormat> {
    format.filter(|format| !format.0.is_empty())
}

const RESERVED_MESSAGE_FIELD_NAMES: &[&str] = &["instance_id"];

fn type_token_name(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "string",
        TypeToken::Bytes => "bytes",
        TypeToken::Time => "time",
        TypeToken::U8 => "u8",
        TypeToken::U16 => "u16",
        TypeToken::U32 => "u32",
        TypeToken::U64 => "u64",
        TypeToken::I8 => "i8",
        TypeToken::I16 => "i16",
        TypeToken::I32 => "i32",
        TypeToken::I64 => "i64",
        TypeToken::F32 => "f32",
        TypeToken::F64 => "f64",
    }
}

fn is_fixed_array_item_copy_primitive(token: &TypeToken) -> bool {
    matches!(
        token,
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
            | TypeToken::F64
    )
}

fn validate_fixed_array_schema(
    schema: &SchemaType,
    path: &str,
    language: PeppygenLanguage,
) -> Result<()> {
    match schema {
        SchemaType::Array(array) => {
            if array.length.is_some() {
                let token = array.items.as_ref().as_type_token().ok_or_else(|| {
                    Error::UnsupportedArrayItemSchema {
                        field: path.to_string(),
                    }
                })?;
                if !is_fixed_array_item_copy_primitive(token) {
                    return Err(Error::UnsupportedFixedArrayItemType {
                        language,
                        field: path.to_string(),
                        item: type_token_name(token),
                    });
                }
            }

            validate_fixed_array_schema(array.items.as_ref(), path, language)
        }
        SchemaType::Object(object) => {
            for (field_name, nested) in &object.fields {
                let nested_path = format!("{path}.{field_name}");
                validate_fixed_array_schema(nested, &nested_path, language)?;
            }
            Ok(())
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

pub fn validate_fixed_length_array_items(
    format: &MessageFormat,
    language: PeppygenLanguage,
) -> Result<()> {
    for (field_name, schema) in &format.0 {
        validate_fixed_array_schema(schema, field_name, language)?;
    }
    Ok(())
}

fn validate_schema_field_names(schema: &SchemaType, path: &str, context: &str) -> Result<()> {
    match schema {
        SchemaType::Object(object) => validate_field_map(object.fields.iter(), path, context),
        SchemaType::Array(array) => {
            validate_schema_field_names(array.items.as_ref(), path, context)
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

fn validate_field_map<'a, I>(fields: I, parent_path: &str, context: &str) -> Result<()>
where
    I: IntoIterator<Item = (&'a String, &'a SchemaType)>,
{
    for (field_name, schema) in fields {
        let path = if parent_path.is_empty() {
            field_name.clone()
        } else {
            format!("{parent_path}.{field_name}")
        };

        if RESERVED_MESSAGE_FIELD_NAMES.contains(&field_name.as_str()) {
            return Err(Error::UnauthorizedMessageFieldName {
                field: field_name.clone(),
                path,
                context: context.to_string(),
            });
        }

        validate_schema_field_names(schema, &path, context)?;
    }

    Ok(())
}

/// Validates payload field names used inside a message format.
///
/// Some names are reserved by transport metadata and cannot be used in payload schemas.
pub fn validate_message_format_field_names(format: &MessageFormat, context: &str) -> Result<()> {
    let normalized_context = if context.trim().is_empty() {
        "message_format"
    } else {
        context
    };
    validate_field_map(format.0.iter(), "", normalized_context)
}

/// Returns the hardcoded cancel-action response format used by both Rust and Python generators.
///
/// The format contains `accepted: bool` and `error_message: Optional[String]`.
pub fn cancel_action_response_format() -> MessageFormat {
    let mut fields = IndexMap::new();
    fields.insert(String::from("accepted"), SchemaType::Type(TypeToken::Bool));
    fields.insert(
        String::from("error_message"),
        SchemaType::Primitive(PrimitiveSchema {
            kind: TypeToken::String,
            optional: true,
        }),
    );
    MessageFormat(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_reserved_message_field_name() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                instance_id: "string",
                value: "u8"
            }
            "#,
        )
        .unwrap();

        let err = validate_message_format_field_names(&format, "test.topic").unwrap_err();

        match err {
            Error::UnauthorizedMessageFieldName {
                field,
                path,
                context,
            } => {
                assert_eq!(field, "instance_id");
                assert_eq!(path, "instance_id");
                assert_eq!(context, "test.topic");
            }
            other => panic!("expected UnauthorizedMessageFieldName, got: {other:?}"),
        }
    }

    #[test]
    fn reject_reserved_nested_message_field_name() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                header: {
                    $type: "object",
                    instance_id: "string"
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_message_format_field_names(&format, "test.topic").unwrap_err();

        match err {
            Error::UnauthorizedMessageFieldName { field, path, .. } => {
                assert_eq!(field, "instance_id");
                assert_eq!(path, "header.instance_id");
            }
            other => panic!("expected UnauthorizedMessageFieldName, got: {other:?}"),
        }
    }

    #[test]
    fn reject_fixed_string_array_for_rust() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                labels: {
                    $type: "array",
                    $items: "string",
                    $length: 3
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_fixed_length_array_items(&format, PeppygenLanguage::Rust).unwrap_err();
        match err {
            Error::UnsupportedFixedArrayItemType {
                language,
                field,
                item,
            } => {
                assert_eq!(language, PeppygenLanguage::Rust);
                assert_eq!(field, "labels");
                assert_eq!(item, "string");
            }
            other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
        }
    }

    #[test]
    fn allow_fixed_i32_array_for_rust() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                samples: {
                    $type: "array",
                    $items: "i32",
                    $length: 4
                }
            }
            "#,
        )
        .unwrap();

        validate_fixed_length_array_items(&format, PeppygenLanguage::Rust)
            .expect("fixed i32 arrays are supported");
    }
}

#[derive(Clone)]
pub struct CapnpSchema {
    file_stem: String,
    schema: String,
}

impl CapnpSchema {
    pub fn new(file_stem: String, schema: String) -> Self {
        Self { file_stem, schema }
    }

    pub fn file_stem(&self) -> &str {
        &self.file_stem
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}
