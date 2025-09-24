use super::types::SubscriberMap;
use config::{
    CallbackName, ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
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
            .enumerate()
            .map(|(i, _t)| {
                let fn_name = Ident::new(&format!("subscribed_topic_{}", i), Span::call_site());
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
            .enumerate()
            .map(|(i, _s)| {
                let fn_name = Ident::new(&format!("subscribed_service_{}", i), Span::call_site());
                quote! { pub fn #fn_name() {} }
            })
            .collect();
        let tokens: TokenStream = quote! { #( #fns )* };
        tokens.to_string()
    }

    fn gen_subscribed_actions(&self, actions: &[SubscriberMap<SubscribedAction>]) -> String {
        let fns: Vec<TokenStream> = actions
            .iter()
            .enumerate()
            .map(|(i, _a)| {
                let fn_name = Ident::new(&format!("subscribed_action_{}", i), Span::call_site());
                quote! { pub fn #fn_name() {} }
            })
            .collect();
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
    use config::MessageFormat;

    use super::*;

    #[test]
    fn test_gen_subscribed_topics() {
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
        assert!(res.contains("pub async fn subscribed_topic_0()"));
        assert!(res.contains("pub async fn subscribed_topic_1()"));
        assert_eq!(
            res.matches("todo!(\"await for message with PMI\")").count(),
            topics.len()
        );
    }
}
