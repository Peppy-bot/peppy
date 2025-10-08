use super::types::InterfaceBackend;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};

/// Python-specific implementation of the interface generator.
pub struct PythonGenerator;

impl PythonGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl InterfaceBackend for PythonGenerator {
    fn exposed_topic(&self, topic: &ExposedTopic) -> String {
        let name = prefixed_name(
            "exposed_topic",
            topic
                .name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| non_empty_str(&topic.topic_type)),
            "topic",
        );
        format!("def {name}():\n    raise NotImplementedError(\"publish PMI topic\")\n")
    }

    fn exposed_service(&self, service: &ExposedService) -> String {
        let name = prefixed_name(
            "exposed_service",
            service
                .name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| non_empty_str(&service.service_type)),
            "service",
        );
        format!("def {name}():\n    raise NotImplementedError(\"expose PMI service\")\n")
    }

    fn exposed_action(&self, action: &ExposedAction) -> String {
        let name = prefixed_name("exposed_action", non_empty_str(&action.name), "action");
        format!("def {name}():\n    raise NotImplementedError(\"expose PMI action\")\n")
    }

    fn subscribed_topic(
        &self,
        topic: &SubscribedTopic,
        arguments: Option<&MessageFormat>,
    ) -> String {
        format!(
            "async def {}():\n    raise NotImplementedError(\"await for message with PMI\")\n",
            topic.callback.as_str()
        )
    }

    fn subscribed_service(
        &self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    ) -> String {
        format!(
            "async def {}():\n    raise NotImplementedError(\"await for service response with PMI\")\n",
            service.callback.as_str()
        )
    }

    fn subscribed_action(
        &self,
        action: &SubscribedAction,
        arguments: Option<&MessageFormat>,
    ) -> String {
        let mut sections = Vec::new();

        if let Some(callback) = action.callback.as_ref() {
            sections.push(format!(
                "async def {}():\n    raise NotImplementedError(\"await for action goal with PMI\")\n",
                callback.as_str()
            ));
        }

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

        sections.join("\n")
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
