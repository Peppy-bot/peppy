#[cfg(test)]
mod tests;

use super::types::{InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage};
use crate::error::Result;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use std::path::Path;

/// Python-specific implementation of the interface generator.
pub struct PythonGenerator {
    sections: Vec<InterfaceArtifact>,
}

impl PythonGenerator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }
}

impl LanguageGenerator for PythonGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) -> Result<()> {
        PythonGenerator::push_section(self, section);
        Ok(())
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let name = prefixed_name("exposed_topic", non_empty_str(topic.name.as_str()), "topic");
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::ExposedTopic,
            format!("def {name}():\n    raise NotImplementedError(\"publish PMI topic\")\n"),
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let name = prefixed_name(
            "exposed_service",
            non_empty_str(service.name.as_str()),
            "service",
        );
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::ExposedService,
            format!("def {name}():\n    raise NotImplementedError(\"expose PMI service\")\n"),
        ));
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let name = prefixed_name("exposed_action", non_empty_str(&action.name), "action");
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            format!("def {name}():\n    raise NotImplementedError(\"expose PMI action\")\n"),
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        _arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::SubscribedTopic,
            format!(
                "async def {}():\n    raise NotImplementedError(\"await for message with PMI\")\n",
                topic.callback.as_str()
            ),
        ));
        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        _arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::SubscribedService,
            format!(
                "async def {}():\n    raise NotImplementedError(\"await for service response with PMI\")\n",
                service.callback.as_str()
            ),
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        _arguments: Option<&SubscribedActionMessage>,
    ) -> Result<()> {
        let mut sections = Vec::new();

        if let Some(callback) = action.feedback_callback.as_ref() {
            sections.push(format!(
                "async def {}():\n    raise NotImplementedError(\"await for action feedback with PMI\")\n",
                callback.as_str()
            ));
        }

        if let Some(callback) = action.results_callback.as_ref() {
            sections.push(format!(
                "async def {}():\n    raise NotImplementedError(\"await for action result with PMI\")\n",
                callback.as_str()
            ));
        }

        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::SubscribedAction,
            sections.join("\n"),
        ));
        Ok(())
    }
    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let _ = to_path;
        let _artifacts = self.into_artifacts();
        // TODO: implement Python project scaffold generation.
        Ok(())
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn prefixed_name(prefix: &str, candidate: Option<&str>, fallback: &str) -> String {
    let fallback_component = match sanitize_component(fallback) {
        component if component.is_empty() => "item".to_string(),
        component => component,
    };

    let maybe_component = candidate.and_then(|value| {
        let sanitized = sanitize_component(value);
        if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        }
    });

    let component = maybe_component
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_component.clone());

    if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
    }
}

fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;

    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_underscore = false;
        } else if !out.is_empty() && !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        } else if out.is_empty() {
            last_was_underscore = true;
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        return String::new();
    }

    if matches!(out.chars().next(), Some(c) if c.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
}
