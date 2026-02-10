#[cfg(test)]
mod tests;

mod actions;
mod build;
mod code_builder;
mod deserialization;
mod parameters;
mod ruff;
pub(crate) mod serialization;
mod services;
mod topics;
mod type_mapping;

use super::naming::{module_name_from_components, sanitize_component, to_camel_case};
use super::types::{
    CapnpSchema, InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage,
};
use crate::error::Result;
use config::encoding::MessageFormatMapper;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use std::collections::HashMap;
use std::path::Path;

/// Schema metadata needed by code generation functions to emit capnp load/init code.
pub(crate) struct PythonSchemaInfo {
    pub file_stem: String,
    pub struct_name: String,
}

/// Python-specific implementation of the interface generator.
#[derive(Default)]
pub struct PythonGenerator {
    sections: Vec<InterfaceArtifact>,
    parameters: config::NodeArguments,
    schemas: HashMap<String, CapnpSchema>,
}

impl PythonGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the node parameters for code generation.
    pub fn set_parameters(&mut self, parameters: config::NodeArguments) {
        self.parameters = parameters;
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }

    /// Registers a Cap'n Proto schema for the given message format and returns
    /// metadata needed by code generation to emit `capnp.load()` / `.new_message()`.
    fn register_schema(
        &mut self,
        schema_key: &str,
        format: &MessageFormat,
    ) -> Result<PythonSchemaInfo> {
        let artifacts = MessageFormatMapper::new(format.clone())
            .map_message_format_to_capnpn()
            .map_err(crate::error::Error::MessageEncoding)?;
        let schema_source = artifacts.encoding_schema().to_string();

        let key_component = sanitize_component(schema_key);
        let base_name = if key_component.is_empty() {
            "message".to_string()
        } else {
            key_component
        };
        let file_stem = if base_name.ends_with("_message") {
            base_name.clone()
        } else {
            format!("{base_name}_message")
        };

        let struct_name = format!("{}Message", to_camel_case(&base_name));
        let schema_text =
            schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        self.schemas.insert(
            file_stem.clone(),
            CapnpSchema::new(file_stem.clone(), schema_text),
        );

        Ok(PythonSchemaInfo {
            file_stem,
            struct_name,
        })
    }
}

impl LanguageGenerator for PythonGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) {
        PythonGenerator::push_section(self, section);
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let schema_info = topic
            .message_format
            .as_ref()
            .map(|fmt| self.register_schema(&topic.name, fmt))
            .transpose()?;

        let code = topics::build_exposed_topic(topic, schema_info.as_ref());
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::ExposedTopic,
            code,
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let request_schema_info = service
            .request_message_format
            .as_ref()
            .filter(|fmt| !fmt.0.is_empty())
            .map(|fmt| self.register_schema(&format!("{}_request", service.name), fmt))
            .transpose()?;

        let response_schema_info = service
            .response_message_format
            .as_ref()
            .filter(|fmt| !fmt.0.is_empty())
            .map(|fmt| self.register_schema(&format!("{}_response", service.name), fmt))
            .transpose()?;

        let code = services::build_exposed_service(
            service,
            request_schema_info.as_ref(),
            response_schema_info.as_ref(),
        );
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::ExposedService,
            code,
        ));
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let code = actions::build_exposed_action(action);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: MessageFormat,
    ) -> Result<()> {
        let schema_info = self.register_schema(&topic.name, &arguments)?;
        let code = topics::build_subscribed_topic(topic, &arguments, Some(&schema_info));
        let module_label = topics::subscribed_topic_module_label(topic);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedTopic,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
    ) -> Result<()> {
        let code =
            services::build_subscribed_service(service, request_arguments, response_arguments);
        let module_label = module_name_from_components(&service.node, &service.name);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedService,
            code,
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: &SubscribedActionMessage,
    ) -> Result<()> {
        let code = actions::build_subscribed_action(action, messages);
        let module_label = module_name_from_components(&action.node, &action.name);
        self.push_section(InterfaceArtifact::from_kind(
            &module_label,
            InterfaceKind::SubscribedAction,
            code,
        ));
        Ok(())
    }

    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let to_path = to_path.as_ref();
        std::fs::create_dir_all(to_path)?;

        build::add_peppylib_dependencies(to_path)?;
        build::add_capnp_schemas(&self.schemas, to_path)?;
        build::add_artifacts_to_lib(to_path, self.sections)?;
        build::add_parameters_to_lib(&self.parameters, to_path)?;

        // Last step, lint the project
        let ruff = ruff::RuffFacade::new()?;
        ruff.check_and_fix(to_path)?;
        ruff.format(to_path)?;

        Ok(())
    }
}
