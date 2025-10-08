use super::types::InterfaceBackend;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator;

impl RustGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl InterfaceBackend for RustGenerator {
    fn exposed_topic(&self, topic: &ExposedTopic) -> String {
        let fn_name = prefixed_ident(
            "exposed_topic",
            topic
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| non_empty_str(&topic.topic_type)),
            "topic",
        );
        let tokens: TokenStream = quote! {
            impl Topics {
                pub fn #fn_name() {
                    todo!("publish PMI topic")
                }
            }
        };
        tokens.to_string()
    }

    fn exposed_service(&self, service: &ExposedService) -> String {
        let fn_name = prefixed_ident(
            "exposed_service",
            service
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| non_empty_str(&service.service_type)),
            "service",
        );
        let tokens: TokenStream = quote! {
            impl Services {
                pub fn #fn_name() {
                    todo!("expose PMI service")
                }
            }
        };
        tokens.to_string()
    }

    fn exposed_action(&self, action: &ExposedAction) -> String {
        let fn_name = prefixed_ident("exposed_action", non_empty_str(&action.name), "action");
        let tokens: TokenStream = quote! {
            impl Actions {
                pub fn #fn_name() {
                    todo!("expose PMI action")
                }
            }
        };
        tokens.to_string()
    }

    fn subscribed_topic(
        &self,
        topic: &SubscribedTopic,
        arguments: Option<&MessageFormat>,
    ) -> String {
        let fn_name = Ident::new(topic.callback.as_str(), Span::call_site());
        let tokens: TokenStream = quote! {
            impl Topics {
                pub async fn #fn_name() {
                    todo!("await for message with PMI")
                }
            }
        };
        tokens.to_string()
    }

    fn subscribed_service(
        &self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    ) -> String {
        let fn_name = Ident::new(service.callback.as_str(), Span::call_site());
        let tokens: TokenStream = quote! {
            impl Services {
                pub async fn #fn_name() {
                    todo!("await for service response with PMI")
                }
            }
        };
        tokens.to_string()
    }

    fn subscribed_action(
        &self,
        action: &SubscribedAction,
        arguments: Option<&MessageFormat>,
    ) -> String {
        let mut fns: Vec<TokenStream> = Vec::new();

        if let Some(callback) = action.callback.as_ref() {
            let fn_name = Ident::new(callback.as_str(), Span::call_site());
            fns.push(quote! {
                pub async fn #fn_name() {
                    todo!("await for action goal with PMI")
                }
            });
        }

        if let Some(callback) = action.feedback_callback.as_ref() {
            let fn_name = Ident::new(callback.as_str(), Span::call_site());
            fns.push(quote! {
                pub async fn #fn_name() {
                    todo!("await for action feedback with PMI")
                }
            });
        }

        if let Some(callback) = action.results_callback.as_ref() {
            let fn_name = Ident::new(callback.as_str(), Span::call_site());
            fns.push(quote! {
                pub async fn #fn_name() {
                    todo!("await for action result with PMI")
                }
            });
        }

        if fns.is_empty() {
            return String::new();
        }

        let tokens: TokenStream = quote! {
            impl Actions {
                #( #fns )*
            }
        };
        tokens.to_string()
    }
}

fn non_empty_str(value: &str) -> Option<&str> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn prefixed_ident(prefix: &str, candidate: Option<&str>, fallback: &str) -> Ident {
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

    let name = if prefix.is_empty() {
        component
    } else {
        format!("{prefix}_{component}")
    };

    Ident::new(&name, Span::call_site())
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

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{
        CallbackName, MessageFormat, SubscribedAction, SubscribedService, SubscribedTopic,
    };

    macro_rules! assert_rendered {
        ($cond:expr, $rendered:expr, $($arg:tt)+) => {
            if !$cond {
                eprintln!("rendered output:\n{}", $rendered);
                panic!($($arg)+);
            }
        };
    }

    fn callback(name: &str) -> CallbackName {
        CallbackName::new(name).expect("valid callback")
    }

    fn dummy_format() -> MessageFormat {
        MessageFormat::default()
    }

    #[test]
    fn subscribed_topic_uses_callback_identifier() {
        let topic = SubscribedTopic {
            node: String::from("vision"),
            name: String::from("camera_feed"),
            tag: String::from("0.1.0"),
            callback: callback("on_camera_feed"),
        };
        let generator = RustGenerator::new();
        let rendered = generator.subscribed_topic(&topic, Some(&dummy_format()));

        assert_rendered!(
            rendered.contains("impl Topics"),
            &rendered,
            "expected impl block for Topics",
        );
        assert_rendered!(
            rendered.contains("pub async fn on_camera_feed"),
            &rendered,
            "expected callback function"
        );
        assert_rendered!(
            rendered.contains("await for message with PMI"),
            &rendered,
            "expected todo message"
        );
        assert_rendered!(
            !rendered.contains("subscribed_topic_"),
            &rendered,
            "callback name should not use prefix fallback"
        );
    }

    #[test]
    fn subscribed_service_uses_callback_identifier() {
        let service = SubscribedService {
            node: String::from("planner"),
            name: String::from("compute_route"),
            tag: String::from("0.7.1"),
            callback: callback("on_compute_route"),
        };
        let generator = RustGenerator::new();
        let rendered = generator.subscribed_service(&service, Some(&dummy_format()));

        assert_rendered!(
            rendered.contains("impl Services"),
            &rendered,
            "expected impl block for Services"
        );
        assert_rendered!(
            rendered.contains("pub async fn on_compute_route"),
            &rendered,
            "expected callback function"
        );
        assert_rendered!(
            rendered.contains("await for service response with PMI"),
            &rendered,
            "expected todo message"
        );
    }

    #[test]
    fn subscribed_action_emits_all_callbacks() {
        let action = SubscribedAction {
            node: String::from("brain"),
            name: String::from("move_arm"),
            tag: String::from("0.1.0"),
            callback: Some(callback("on_move_arm_goal")),
            feedback_callback: Some(callback("on_move_arm_feedback")),
            results_callback: Some(callback("on_move_arm_result")),
        };
        let generator = RustGenerator::new();
        let rendered = generator.subscribed_action(&action, Some(&dummy_format()));

        assert_rendered!(
            rendered.contains("impl Actions"),
            &rendered,
            "expected impl block for Actions"
        );
        for expected in [
            "pub async fn on_move_arm_goal",
            "pub async fn on_move_arm_feedback",
            "pub async fn on_move_arm_result",
        ] {
            assert_rendered!(
                rendered.contains(expected),
                &rendered,
                "expected `{expected}` in rendered"
            );
        }
        assert_rendered!(
            rendered.matches("await for action").count() == 3,
            &rendered,
            "expected todo message for each callback"
        );
    }

    #[test]
    fn subscribed_action_without_callbacks_returns_empty_string() {
        let action = SubscribedAction {
            node: String::from("brain"),
            name: String::from("idle"),
            tag: String::from("0.1.0"),
            callback: None,
            feedback_callback: None,
            results_callback: None,
        };
        let generator = RustGenerator::new();

        assert_eq!(
            generator.subscribed_action(&action, Some(&dummy_format())),
            ""
        );
    }

    #[test]
    fn prefixed_ident_sanitizes_candidate_and_fallback() {
        let candidate = prefixed_ident("exposed_topic", Some(" My-Topic "), "topic");
        assert_eq!(candidate.to_string(), "exposed_topic_my_topic");

        let fallback = prefixed_ident("exposed_service", None, "@@@");
        assert_eq!(fallback.to_string(), "exposed_service_item");

        let starts_with_digit = prefixed_ident("", Some("42meaning"), "unused");
        assert_eq!(starts_with_digit.to_string(), "_42meaning");
    }
}
