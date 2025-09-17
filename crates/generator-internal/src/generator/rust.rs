use config::{
    ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
    SubscribedTopic,
};
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

use super::builder::InterfaceGenerator;

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator;

impl RustGenerator {
    pub fn new() -> Self {
        Self
    }
}

impl InterfaceGenerator for RustGenerator {
    fn gen_subscribed_topics(&self, topics: &[SubscribedTopic]) -> String {
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

    fn gen_subscribed_services(&self, services: &[SubscribedService]) -> String {
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

    fn gen_subscribed_actions(&self, actions: &[SubscribedAction]) -> String {
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
    use super::*;

    #[test]
    fn test_gen_subscribed_topics() {
        let topics = [
            SubscribedTopic {
                node: String::from("node_alpha"),
                name: String::from("topic_alpha"),
                tag: String::from("alpha"),
                namespace: String::from("foo"),
                callback: String::from("on_topic_alpha"),
                optional: Some(false),
            },
            SubscribedTopic {
                node: String::from("node_beta"),
                name: String::from("topic_beta"),
                tag: String::from("beta"),
                namespace: String::from("bar"),
                callback: String::from("on_topic_beta"),
                optional: Some(true),
            },
        ];
        let generator = RustGenerator::new();
        let res = generator.gen_subscribed_topics(&topics);
        dbg!(res);
    }
}
