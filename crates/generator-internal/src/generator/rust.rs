use super::types::{InterfaceArtifact, InterfaceBackend, InterfaceKind};
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }

    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }
}

impl InterfaceBackend for RustGenerator {
    fn add_exposed_topic(&mut self, topic: &ExposedTopic) {
        let fn_name = prefixed_ident("exposed_topic", non_empty_str(topic.name.as_str()), "topic");
        let tokens: TokenStream = quote! {
            impl Topics {
                pub fn #fn_name() {
                    todo!("publish PMI topic")
                }
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::ExposedTopic,
            tokens.to_string(),
        ));
    }

    fn add_exposed_service(&mut self, service: &ExposedService) {
        let fn_name = prefixed_ident(
            "exposed_service",
            service.name.as_ref().and_then(|name| non_empty_str(name)),
            "service",
        );
        let tokens: TokenStream = quote! {
            impl Services {
                pub fn #fn_name() {
                    todo!("expose PMI service")
                }
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::ExposedService,
            tokens.to_string(),
        ));
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) {
        let fn_name = prefixed_ident("exposed_action", non_empty_str(&action.name), "action");
        let tokens: TokenStream = quote! {
            impl Actions {
                pub fn #fn_name() {
                    todo!("expose PMI action")
                }
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::ExposedAction,
            tokens.to_string(),
        ));
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        _arguments: Option<&MessageFormat>,
    ) {
        let fn_name = Ident::new(topic.callback.as_str(), Span::call_site());
        let tokens: TokenStream = quote! {
            impl Topics {
                pub async fn #fn_name() {
                    todo!("await for message with PMI")
                }
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::SubscribedTopic,
            tokens.to_string(),
        ));
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        _arguments: Option<&MessageFormat>,
    ) {
        let fn_name = Ident::new(service.callback.as_str(), Span::call_site());
        let tokens: TokenStream = quote! {
            impl Services {
                pub async fn #fn_name() {
                    todo!("await for service response with PMI")
                }
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::SubscribedService,
            tokens.to_string(),
        ));
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        _arguments: Option<&MessageFormat>,
    ) {
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
            return;
        }

        let tokens: TokenStream = quote! {
            impl Actions {
                #( #fns )*
            }
        };
        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::SubscribedAction,
            tokens.to_string(),
        ));
    }

    fn finish(self: Box<Self>) -> Vec<InterfaceArtifact> {
        let inner = *self;
        inner.into_artifacts()
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
    use config::node::ExposedTopic;
    use config::node::{CallbackName, SubscribedAction, SubscribedService, SubscribedTopic};
    use config::peppy_config::Deployment;

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

    fn single_artifact(artifacts: Vec<String>) -> String {
        assert_eq!(
            artifacts.len(),
            1,
            "expected a single generated artifact, got {}",
            artifacts.len()
        );
        artifacts.into_iter().next().expect("artifact is present")
    }

    // Example of an exposed topic:
    // {
    //   name: "stream",
    //   qos_profile: "sensor_data",
    //   message_format: {
    //     header: {
    //       stamp: "time",
    //       frame_id: "u32",
    //     },
    //     encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
    //     width: "u32",
    //     height: "u32",
    //     image: {
    //       type: "array",
    //       items: "u8",
    //       length: 3
    //     },
    //   },
    // }
    #[test]
    fn exposed_topic_gen_calling_code() {
        let deployment = r#"
        {
          name: "uvc_camera",
          instances: [
            {
              namespace: "/camera/right",
              parameters: {
                device: {
                  physical: "/dev/video_right",
                  sim: "mujoco:camera_right",
                  priority: "physical"
                },
                video: {
                  frame_rate: 30,
                  resolution: {
                    width: 1920,
                    height: 1080,
                  },
                  encoding: "yuyv",
                },
              }
            }
          ]
        }
        "#;

        let topic = r#"
            {
                name: "uvc_camera",
                qos_profile: "sensor_data",
                message_format: {
                    header: {
                        stamp: "time",
                        frame_id: "u32",
                    },
                    encoding: "string",
                    width: "u32",
                    height: "u32",
                    image: {
                        type: "array",
                        items: "u8",
                        length: 3,
                    },
                },
            }
        "#;
        //let deployment: Deployment = serde_json5::from_str(deployment).unwrap();
        let topic: ExposedTopic = serde_json5::from_str(topic).unwrap();

        let generator = RustGenerator::new();
        let artifacts = generator.into_artifacts();
        println!("{:#?}", topic);
        todo!("Finish")
    }

    // #[test]
    // fn subscribed_topic_uses_callback_identifier() {
    //     let topic = SubscribedTopic {
    //         node: String::from("vision"),
    //         name: String::from("camera_feed"),
    //         tag: String::from("0.1.0"),
    //         callback: callback("on_camera_feed"),
    //     };
    //     let mut generator = RustGenerator::new();
    //     generator.add_subscribed_topic(&topic, Some(&dummy_format()));
    //     let rendered = single_artifact(generator.into_artifacts());

    //     assert_rendered!(
    //         rendered.contains("impl Topics"),
    //         &rendered,
    //         "expected impl block for Topics",
    //     );
    //     assert_rendered!(
    //         rendered.contains("pub async fn on_camera_feed"),
    //         &rendered,
    //         "expected callback function"
    //     );
    //     assert_rendered!(
    //         rendered.contains("await for message with PMI"),
    //         &rendered,
    //         "expected todo message"
    //     );
    //     assert_rendered!(
    //         !rendered.contains("subscribed_topic_"),
    //         &rendered,
    //         "callback name should not use prefix fallback"
    //     );
    // }

    // #[test]
    // fn subscribed_service_uses_callback_identifier() {
    //     let service = SubscribedService {
    //         node: String::from("planner"),
    //         name: String::from("compute_route"),
    //         tag: String::from("0.7.1"),
    //         callback: callback("on_compute_route"),
    //     };
    //     let mut generator = RustGenerator::new();
    //     generator.add_subscribed_service(&service, Some(&dummy_format()));
    //     let rendered = single_artifact(generator.into_artifacts());

    //     assert_rendered!(
    //         rendered.contains("impl Services"),
    //         &rendered,
    //         "expected impl block for Services"
    //     );
    //     assert_rendered!(
    //         rendered.contains("pub async fn on_compute_route"),
    //         &rendered,
    //         "expected callback function"
    //     );
    //     assert_rendered!(
    //         rendered.contains("await for service response with PMI"),
    //         &rendered,
    //         "expected todo message"
    //     );
    // }

    // #[test]
    // fn subscribed_action_emits_all_callbacks() {
    //     let action = SubscribedAction {
    //         node: String::from("brain"),
    //         name: String::from("move_arm"),
    //         tag: String::from("0.1.0"),
    //         callback: Some(callback("on_move_arm_goal")),
    //         feedback_callback: Some(callback("on_move_arm_feedback")),
    //         results_callback: Some(callback("on_move_arm_result")),
    //     };
    //     let mut generator = RustGenerator::new();
    //     generator.add_subscribed_action(&action, Some(&dummy_format()));
    //     let rendered = single_artifact(generator.into_artifacts());

    //     assert_rendered!(
    //         rendered.contains("impl Actions"),
    //         &rendered,
    //         "expected impl block for Actions"
    //     );
    //     for expected in [
    //         "pub async fn on_move_arm_goal",
    //         "pub async fn on_move_arm_feedback",
    //         "pub async fn on_move_arm_result",
    //     ] {
    //         assert_rendered!(
    //             rendered.contains(expected),
    //             &rendered,
    //             "expected `{expected}` in rendered"
    //         );
    //     }
    //     assert_rendered!(
    //         rendered.matches("await for action").count() == 3,
    //         &rendered,
    //         "expected todo message for each callback"
    //     );
    // }

    // #[test]
    // fn subscribed_action_without_callbacks_returns_empty_string() {
    //     let action = SubscribedAction {
    //         node: String::from("brain"),
    //         name: String::from("idle"),
    //         tag: String::from("0.1.0"),
    //         callback: None,
    //         feedback_callback: None,
    //         results_callback: None,
    //     };
    //     let mut generator = RustGenerator::new();
    //     generator.add_subscribed_action(&action, Some(&dummy_format()));

    //     assert!(
    //         generator.into_artifacts().is_empty(),
    //         "expected no generated artifacts when callbacks are absent"
    //     );
    // }

    // #[test]
    // fn prefixed_ident_sanitizes_candidate_and_fallback() {
    //     let candidate = prefixed_ident("exposed_topic", Some(" My-Topic "), "topic");
    //     assert_eq!(candidate.to_string(), "exposed_topic_my_topic");

    //     let fallback = prefixed_ident("exposed_service", None, "@@@");
    //     assert_eq!(fallback.to_string(), "exposed_service_item");

    //     let starts_with_digit = prefixed_ident("", Some("42meaning"), "unused");
    //     assert_eq!(starts_with_digit.to_string(), "_42meaning");
    // }
}
