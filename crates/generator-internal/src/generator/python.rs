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

use super::naming::{resolve_schema_file_stem, to_camel_case};
use super::types::{
    CapnpSchema, ConsumedActionMessage, ContractOrigin, DependencyContext, InterfaceArtifact,
    InterfaceKind, LanguageGenerator, goal_action_response_format, non_empty_message_format,
    scoped_schema_key, validate_fixed_length_array_items, validate_generated_type_name_collisions,
    validate_message_format_field_names,
};
use crate::error::Result;
use config::node::{
    ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService,
};
use encoding::MessageFormatMapper;
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
        origin: Option<&ContractOrigin>,
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
        topic: &NativeEmittedTopic,
        origin: Option<&ContractOrigin>,
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
        service: &NativeExposedService,
        origin: Option<&ContractOrigin>,
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
        action: &NativeExposedAction,
        origin: Option<&ContractOrigin>,
    ) -> Result<()> {
        let goal_request_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_goal_request", action.name)),
            action
                .goal_service
                .as_ref()
                .and_then(|goal_service| goal_service.request_message_format.as_ref()),
        )?;
        // The goal acknowledgement is framework-owned ({accepted, error_message}).
        let goal_response_fmt = goal_action_response_format();
        let goal_response_schema_info = self.register_optional_schema(
            scoped_schema_key(origin, &format!("{}_goal_response", action.name)),
            Some(&goal_response_fmt),
        )?;

        // The cancel-ack reply is encoded by the peppylib engine (a fixed
        // format), so the exposed server needs no per-action cancel-response
        // schema.

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
        let schema_key = crate::generator::naming::consumed_topic_schema_key(
            topic.link_id.as_str(),
            topic.name.as_str(),
        );
        let schema_info = self.register_schema(&schema_key, &arguments)?;
        let code = topics::build_consumed_topic(topic, &arguments, &schema_info, dependency)?;
        self.push_section(InterfaceArtifact {
            module_path: vec![topic.link_id.clone(), topic.name.clone()],
            kind: InterfaceKind::ConsumedTopic,
            code_output: code,
        });
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
        self.push_section(InterfaceArtifact {
            module_path: vec![service.link_id.clone(), service.name.clone()],
            kind: InterfaceKind::ConsumedService,
            code_output: code,
        });
        Ok(())
    }

    fn add_peer_emitted_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
    ) -> Result<()> {
        let schema_key = crate::generator::naming::peer_schema_key(&peer.link_id, &topic.name);
        let schema_info = topic
            .message_format
            .as_ref()
            .map(|fmt| self.register_schema(&schema_key, fmt))
            .transpose()?;

        let code = topics::build_peer_emitted_topic(topic, schema_info.as_ref(), peer)?;
        let module_path = peer.module_path_for(&topic.name);
        crate::generator::types::ensure_no_peer_collision(
            &self.sections,
            &module_path,
            peer,
            topic,
        )?;
        self.push_section(InterfaceArtifact {
            module_path,
            kind: InterfaceKind::PeerEmittedTopic,
            code_output: code,
        });
        Ok(())
    }

    fn add_peer_consumed_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        peer: &crate::generator::types::PeerContext,
    ) -> Result<()> {
        let schema_key = crate::generator::naming::peer_schema_key(&peer.link_id, &topic.name);
        let arguments = topic.message_format.clone().ok_or_else(|| {
            crate::error::Error::PeerTopicMissingMessageFormat {
                link_id: peer.link_id.clone(),
                topic: topic.name.clone(),
            }
        })?;
        let schema_info = self.register_schema(&schema_key, &arguments)?;

        let code = topics::build_peer_consumed_topic(topic, &arguments, &schema_info, peer)?;
        let module_path = peer.module_path_for(&topic.name);
        crate::generator::types::ensure_no_peer_collision(
            &self.sections,
            &module_path,
            peer,
            topic,
        )?;
        self.push_section(InterfaceArtifact {
            module_path,
            kind: InterfaceKind::PeerConsumedTopic,
            code_output: code,
        });
        Ok(())
    }

    fn add_observed_topic(
        &mut self,
        topic: &NativeEmittedTopic,
        observer: &crate::generator::types::PeerContext,
    ) -> Result<()> {
        // Observer slot link_ids are unique across the node, so the peer
        // schema-key scheme yields a collision-free key for an observer too.
        let schema_key = crate::generator::naming::peer_schema_key(&observer.link_id, &topic.name);
        let arguments = topic.message_format.clone().ok_or_else(|| {
            crate::error::Error::PeerTopicMissingMessageFormat {
                link_id: observer.link_id.clone(),
                topic: topic.name.clone(),
            }
        })?;
        let schema_info = self.register_schema(&schema_key, &arguments)?;

        let code = topics::build_observed_topic(topic, &arguments, &schema_info, observer)?;
        let module_path = observer.module_path_for(&topic.name);
        crate::generator::types::ensure_no_peer_collision(
            &self.sections,
            &module_path,
            observer,
            topic,
        )?;
        self.push_section(InterfaceArtifact {
            module_path,
            kind: InterfaceKind::ObservedTopic,
            code_output: code,
        });
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
        // The goal acknowledgement is framework-owned ({accepted, error_message}).
        let goal_response_fmt = goal_action_response_format();
        let goal_response_schema_info = self.register_optional_schema(
            &action_schema_keys.goal_response,
            Some(&goal_response_fmt),
        )?;

        // The cancel reply is the framework-owned cancel-ack, decoded Rust-side
        // by the peppylib binding (`cancel_goal` returns a typed reply), so the
        // generated client needs no per-action cancel payload schema.
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
                cancel_response: None,
                feedback: feedback_schema_info.as_ref(),
                result_response: result_response_schema_info.as_ref(),
            },
            dependency,
        )?;
        self.push_section(InterfaceArtifact {
            module_path: vec![action.link_id.clone(), action.name.clone()],
            kind: InterfaceKind::ConsumedAction,
            code_output: code,
        });
        Ok(())
    }

    fn build(
        self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &daemon_config::consts::PeppyDirs,
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
