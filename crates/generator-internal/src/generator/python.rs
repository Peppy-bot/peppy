#[cfg(test)]
mod tests;

mod actions;
mod code_builder;
mod deserialization;
mod identifiers;
mod parameters;
mod ruff;
mod scaffold;
pub(crate) mod serialization;
mod services;
mod topics;
mod type_mapping;

use super::naming::{module_name_from_components, resolve_schema_file_stem, to_camel_case};
use super::types::{
    CapnpSchema, ConsumedActionMessage, DependencyContext, InterfaceArtifact, InterfaceKind,
    InterfaceOrigin, LanguageGenerator, cancel_action_response_format, non_empty_message_format,
    scoped_schema_key, validate_fixed_length_array_items, validate_generated_type_name_collisions,
    validate_message_format_field_names,
};
use crate::error::{Error, Result};
use config::encoding::MessageFormatMapper;
use config::node::{
    ConsumedAction, ConsumedService, ConsumedTopic, EmittedTopic, ExposedAction, ExposedService,
    MessageFormat,
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
    parameters: config::ParameterSchema,
    schemas: HashMap<String, CapnpSchema>,
    is_container: bool,
}

impl PythonGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    fn make_artifact(
        &self,
        leaf_name: &str,
        origin: Option<&InterfaceOrigin>,
        kind: InterfaceKind,
        code_output: String,
    ) -> InterfaceArtifact {
        InterfaceArtifact::for_leaf(origin, leaf_name, kind, code_output)
    }

    /// Sets the node parameters for code generation.
    pub fn set_parameters(&mut self, parameters: config::ParameterSchema) {
        self.parameters = parameters;
    }

    /// Marks this generator as targeting a container deployment.
    ///
    /// When `true`, the Linux cross-compiled `.so` is deployed; otherwise the
    /// host platform's `.so` is used.
    pub fn set_container(&mut self, is_container: bool) {
        self.is_container = is_container;
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
        validate_message_format_field_names(format, schema_key)?;
        validate_fixed_length_array_items(format)?;
        validate_generated_type_name_collisions(format, "Message")?;

        let artifacts = MessageFormatMapper::new(schema_key, format.clone())
            .map_message_format_to_capnpn()
            .map_err(crate::error::Error::MessageEncoding)?;
        let schema_source = artifacts.encoding_schema().to_string();

        let resolved = resolve_schema_file_stem(schema_key);
        let struct_name = format!("{}Message", to_camel_case(&resolved.base_name));
        let schema_text =
            schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let struct_module = crate::generator::naming::normalize_snake_case(&struct_name);
        self.schemas.insert(
            resolved.file_stem.clone(),
            CapnpSchema::new(resolved.file_stem.clone(), struct_module, schema_text),
        );

        Ok(PythonSchemaInfo {
            file_stem: resolved.file_stem,
            struct_name,
        })
    }

    fn register_optional_schema(
        &mut self,
        schema_key: impl AsRef<str>,
        format: Option<&MessageFormat>,
    ) -> Result<Option<PythonSchemaInfo>> {
        format
            .filter(|format| !format.0.is_empty())
            .map(|format| self.register_schema(schema_key.as_ref(), format))
            .transpose()
    }
}

impl LanguageGenerator for PythonGenerator {
    fn add_emitted_topic(
        &mut self,
        topic: &EmittedTopic,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let scoped_key = scoped_schema_key(origin, &topic.name);
        let schema_info = topic
            .message_format
            .as_ref()
            .map(|fmt| self.register_schema(&scoped_key, fmt))
            .transpose()?;

        let code = topics::build_emitted_topic(topic, schema_info.as_ref(), origin)?;
        self.push_section(self.make_artifact(
            &topic.name,
            origin,
            InterfaceKind::EmittedTopic,
            code,
        ));
        Ok(())
    }

    fn add_exposed_service(
        &mut self,
        service: &ExposedService,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let request_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_request", service.name)),
            service.request_message_format.as_ref(),
        )?;
        let response_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_response", service.name)),
            service.response_message_format.as_ref(),
        )?;

        let code = services::build_exposed_service(
            service,
            request_schema_info.as_ref(),
            response_schema_info.as_ref(),
            origin,
        )?;
        self.push_section(self.make_artifact(
            &service.name,
            origin,
            InterfaceKind::ExposedService,
            code,
        ));
        Ok(())
    }

    fn add_exposed_action(
        &mut self,
        action: &ExposedAction,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()> {
        let goal_request_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_goal_request", action.name)),
            action
                .goal_service
                .as_ref()
                .and_then(|goal_service| goal_service.request_message_format.as_ref()),
        )?;
        let goal_response_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_goal_response", action.name)),
            action
                .goal_service
                .as_ref()
                .and_then(|goal_service| goal_service.response_message_format.as_ref()),
        )?;

        // The cancel-ack reply is encoded by the peppylib engine (a fixed
        // format), so the exposed server no longer needs a per-action
        // cancel-response schema.

        let result_response_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_result_response", action.name)),
            action
                .result_service
                .as_ref()
                .and_then(|result_service| result_service.response_message_format.as_ref()),
        )?;
        let feedback_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_feedback", action.name)),
            action
                .feedback_topic
                .as_ref()
                .and_then(|feedback_topic| feedback_topic.message_format.as_ref()),
        )?;

        let code = actions::build_exposed_action(
            action,
            goal_request_schema_info.as_ref(),
            goal_response_schema_info.as_ref(),
            result_response_schema_info.as_ref(),
            feedback_schema_info.as_ref(),
            origin,
        )?;
        self.push_section(self.make_artifact(
            &action.name,
            origin,
            InterfaceKind::ExposedAction,
            code,
        ));
        Ok(())
    }

    fn add_consumed_topic(
        &mut self,
        topic: &ConsumedTopic,
        arguments: MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let ConsumedTopic::Linked(linked) = topic else {
            return Err(Error::InvariantViolation {
                context: "add_consumed_topic called with ConsumedTopic::External; use add_external_consumed_topic instead".into(),
            });
        };
        let schema_key = crate::generator::naming::consumed_topic_schema_key(
            linked.link_id.as_str(),
            linked.name.as_str(),
        );
        let schema_info = self.register_schema(&schema_key, &arguments)?;
        let code = topics::build_consumed_topic(topic, &arguments, &schema_info, dependency)?;
        let module_label = module_name_from_components(&linked.link_id, &linked.name);
        self.push_section(self.make_artifact(
            &module_label,
            None,
            InterfaceKind::ConsumedTopic,
            code,
        ));
        Ok(())
    }

    fn add_external_consumed_topic(&mut self, name: &str, arguments: MessageFormat) -> Result<()> {
        let schema_key = crate::generator::naming::consumed_topic_schema_key("", name);
        let schema_info = self.register_schema(&schema_key, &arguments)?;
        let code = topics::build_external_consumed_topic(name, &arguments, &schema_info)?;
        let module_label = name.trim().to_string();
        self.push_section(self.make_artifact(
            &module_label,
            None,
            InterfaceKind::ConsumedTopic,
            code,
        ));
        Ok(())
    }

    fn add_consumed_service(
        &mut self,
        service: &ConsumedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let producer_name = dependency.producer_name.as_str();
        let request_schema_info = self.register_optional_schema(
            crate::generator::naming::consumed_service_request_schema_key(
                producer_name,
                &service.name,
            ),
            non_empty_message_format(Some(request_arguments)),
        )?;
        let response_schema_info = self.register_optional_schema(
            crate::generator::naming::consumed_service_response_schema_key(
                producer_name,
                &service.name,
            ),
            non_empty_message_format(Some(response_arguments)),
        )?;

        let code = services::build_consumed_service(
            service,
            request_arguments,
            response_arguments,
            request_schema_info.as_ref(),
            response_schema_info.as_ref(),
            dependency,
        )?;
        let module_label = module_name_from_components(&service.link_id, &service.name);
        self.push_section(self.make_artifact(
            &module_label,
            None,
            InterfaceKind::ConsumedService,
            code,
        ));
        Ok(())
    }

    fn add_consumed_action(
        &mut self,
        action: &ConsumedAction,
        messages: &ConsumedActionMessage,
        dependency: &DependencyContext,
    ) -> Result<()> {
        let action_schema_keys = crate::generator::naming::consumed_action_schema_keys(
            dependency.producer_name.as_str(),
            action.name.as_str(),
        );
        let goal_request_schema_info = self.register_optional_schema(
            &action_schema_keys.goal_request,
            messages.goal_request.as_ref(),
        )?;
        let goal_response_schema_info = self.register_optional_schema(
            &action_schema_keys.goal_response,
            messages.goal_response.as_ref(),
        )?;

        let cancel_format = cancel_action_response_format();
        let cancel_response_schema_info =
            Some(self.register_schema(&action_schema_keys.cancel_response, &cancel_format)?);

        let feedback_schema_info = self
            .register_optional_schema(&action_schema_keys.feedback, messages.feedback.as_ref())?;
        let result_response_schema_info = self.register_optional_schema(
            &action_schema_keys.result_response,
            messages.result_response.as_ref(),
        )?;

        let code = actions::build_consumed_action(
            action,
            messages,
            actions::ConsumedActionSchemaInfo {
                goal_request: goal_request_schema_info.as_ref(),
                goal_response: goal_response_schema_info.as_ref(),
                cancel_response: cancel_response_schema_info.as_ref(),
                feedback: feedback_schema_info.as_ref(),
                result_response: result_response_schema_info.as_ref(),
            },
            dependency,
        )?;
        let module_label = module_name_from_components(&action.link_id, &action.name);
        self.push_section(self.make_artifact(
            &module_label,
            None,
            InterfaceKind::ConsumedAction,
            code,
        ));
        Ok(())
    }

    fn build(
        self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &config::consts::PeppyDirs,
        _deploy_mode: crate::generator::common::CrateDeployMode,
    ) -> Result<()> {
        let to_path = to_path.as_ref();
        std::fs::create_dir_all(to_path)?;

        scaffold::add_peppylib_dependencies(to_path, peppy_dirs, self.is_container)?;
        scaffold::add_capnp_schemas(&self.schemas, to_path)?;
        scaffold::add_artifacts_to_lib(to_path, self.sections)?;
        scaffold::add_parameters_to_lib(&self.parameters, to_path)?;

        // Last step, lint the project
        let ruff = ruff::RuffFacade::new()?;
        ruff.check_and_fix(to_path)?;
        ruff.format(to_path)?;

        Ok(())
    }
}
