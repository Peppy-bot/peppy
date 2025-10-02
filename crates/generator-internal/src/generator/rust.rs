use super::types::SubscriberMap;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
    SubscribedTopic,
};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

use super::types::InterfaceGenerator;

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator;

impl RustGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl InterfaceGenerator for RustGenerator {
    fn gen_subscribed_topics(&self, topics: &[SubscriberMap<SubscribedTopic>]) -> String {
        // Placeholder: produce a simple Rust fn per topic name for now
        let fns: Vec<TokenStream> = topics
            .iter()
            .map(|topic| {
                let callback = topic.subscriber().callback.as_str();
                let fn_name = Ident::new(callback, Span::call_site());
                quote! {
                    pub async fn #fn_name() {
                        todo!("await for message with PMI")
                    }
                }
            })
            .collect();
        let tokens: TokenStream = quote! {
            use pmi::{MessagingEngineContext, Messenger};

            impl Topics {
                #( #fns )*
            }
        };
        let file =
            syn::parse2::<syn::File>(tokens).expect("failed to parse generated subscribed topics");

        prettyplease::unparse(&file)
    }

    fn gen_subscribed_services(&self, services: &[SubscriberMap<SubscribedService>]) -> String {
        let fns: Vec<TokenStream> = services
            .iter()
            .map(|service| {
                let callback = service.subscriber().callback.as_str();
                let fn_name = Ident::new(callback, Span::call_site());
                quote! {
                    pub async fn #fn_name() {
                        todo!("await for service response with PMI")
                    }
                }
            })
            .collect();
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }

    fn gen_subscribed_actions(&self, actions: &[SubscriberMap<SubscribedAction>]) -> String {
        let mut fns: Vec<TokenStream> = Vec::new();
        for action in actions.iter() {
            let subscriber = action.subscriber();
            if let Some(callback) = subscriber.callback.as_ref() {
                let fn_name = Ident::new(callback.as_str(), Span::call_site());
                fns.push(quote! {
                    pub async fn #fn_name() {
                        todo!("await for action goal with PMI")
                    }
                });
            }
            if let Some(callback) = subscriber.feedback_callback.as_ref() {
                let fn_name = Ident::new(callback.as_str(), Span::call_site());
                fns.push(quote! {
                    pub async fn #fn_name() {
                        todo!("await for action feedback with PMI")
                    }
                });
            }
            if let Some(callback) = subscriber.results_callback.as_ref() {
                let fn_name = Ident::new(callback.as_str(), Span::call_site());
                fns.push(quote! {
                    pub async fn #fn_name() {
                        todo!("await for action result with PMI")
                    }
                });
            }
        }
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }

    fn gen_exposed_topics(&self, topics: &[ExposedTopic]) -> String {
        let fns: Vec<TokenStream> = topics
            .iter()
            .enumerate()
            .map(|(i, _t)| {
                let fn_name = Ident::new(&format!("exposed_topic_{}", i), Span::call_site());
                quote! { pub fn #fn_name() {} }
            })
            .collect();
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }

    fn gen_exposed_services(&self, services: &[ExposedService]) -> String {
        let fns: Vec<TokenStream> = services
            .iter()
            .enumerate()
            .map(|(i, _s)| {
                let fn_name = Ident::new(&format!("exposed_service_{}", i), Span::call_site());
                quote! { pub fn #fn_name() {} }
            })
            .collect();
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }

    fn gen_exposed_actions(&self, actions: &[ExposedAction]) -> String {
        let fns: Vec<TokenStream> = actions
            .iter()
            .enumerate()
            .map(|(i, _a)| {
                let fn_name = Ident::new(&format!("exposed_action_{}", i), Span::call_site());
                quote! { pub fn #fn_name() {} }
            })
            .collect();
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use config::node::{CallbackName, MessageFormat};

    use super::*;

    #[test]
    fn test_gen_subscribed_topics() {
        todo!(
            "The subscribed actions should map to an exposes_to action that exposes the types required for the creation of the callback functions"
        );
        let topics = [
            SubscriberMap::new(
                SubscribedTopic {
                    node: String::from("node_alpha"),
                    name: String::from("topic_alpha"),
                    tag: String::from("alpha"),
                    callback: CallbackName::new("on_topic_alpha").expect("valid callback"),
                    optional: Some(false),
                },
                MessageFormat::default(),
            ),
            SubscriberMap::new(
                SubscribedTopic {
                    node: String::from("node_beta"),
                    name: String::from("topic_beta"),
                    tag: String::from("beta"),
                    callback: CallbackName::new("on_topic_beta").expect("valid callback"),
                    optional: Some(true),
                },
                MessageFormat::default(),
            ),
        ];
        let generator = RustGenerator::new();
        let res = generator.gen_subscribed_topics(&topics);
        assert!(res.contains("use pmi::{MessagingEngineContext, Messenger};"));
        assert!(res.contains("impl Topics"));
        assert!(res.contains("pub async fn on_topic_alpha("));
        assert!(res.contains("pub async fn on_topic_beta("));
        assert!(!res.contains("subscribed_topic_"));
        assert_eq!(
            res.matches("todo!(\"await for message with PMI\")").count(),
            topics.len()
        );
    }

    #[test]
    fn test_gen_subscribed_services() {
        todo!(
            "The subscribed actions should map to an exposes_to action that exposes the types required for the creation of the callback functions"
        );
        let services_json = r#"[
            {
                node: "node_alpha",
                name: "service_alpha",
                tag: "alpha",
                callback: "on_service_alpha"
            }
        ]"#;

        let service_values: Vec<SubscribedService> = serde_json5::from_str(services_json).unwrap();
        let services: Vec<_> = service_values
            .into_iter()
            .map(|service| SubscriberMap::new(service, MessageFormat::default()))
            .collect();
        let generator = RustGenerator::new();
        let res = generator.gen_subscribed_services(&services);
        assert!(res.contains("pub async fn on_service_alpha"));
        assert!(res.contains("await for service response with PMI"));
    }

    #[test]
    fn test_gen_subscribed_actions() {
        todo!(
            "The subscribed actions should map to an exposes_to action that exposes the types required for the creation of the callback functions"
        );
        let actions_json = r#"[
            {
                node: "brain",
                name: "move_arm",
                tag: "0.1.0",
                callback: "on_move_arm_goal",
                feedback_callback: "on_move_arm_feedback",
            },
            {
                node: "brain",
                name: "move_leg",
                tag: "0.1.0",
                results_callback: "on_move_leg_result",
                optional: true,
            }
        ]"#;

        let action_values: Vec<SubscribedAction> = serde_json5::from_str(actions_json).unwrap();
        let actions: Vec<_> = action_values
            .into_iter()
            .map(|action| SubscriberMap::new(action, MessageFormat::default()))
            .collect();
        let generator = RustGenerator::new();
        let res = generator.gen_subscribed_actions(&actions);
        assert!(res.contains("pub async fn on_move_arm_goal"));
        assert!(res.contains("pub async fn on_move_arm_feedback"));
        assert!(res.contains("pub async fn on_move_leg_result"));
        assert!(!res.contains("subscribed_action_"));
        assert_eq!(res.matches("await for action").count(), 3);
    }
}
