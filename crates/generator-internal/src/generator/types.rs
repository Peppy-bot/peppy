use crate::error::{Error, Result};
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, PrimitiveSchema, SchemaType,
    SubscribedAction, SubscribedService, SubscribedTopic, TypeToken,
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
    fn build(self, to_path: impl AsRef<Path>) -> Result<()>;
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
