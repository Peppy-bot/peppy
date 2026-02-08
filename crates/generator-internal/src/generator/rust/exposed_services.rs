use super::type_mapping::render_tokens;
use crate::generator::types::{InterfaceArtifact, InterfaceKind};
use proc_macro2::TokenStream;
use quote::quote;

pub(super) struct ServiceModule {
    pub(super) module_name: String,
    pub(super) tokens: Vec<TokenStream>,
}

pub(super) struct ExposedServicesModule {
    services: Vec<ServiceModule>,
}

impl ExposedServicesModule {
    pub(super) fn new() -> Self {
        Self {
            services: Vec::new(),
        }
    }

    pub(super) fn push_service(&mut self, module_name: String, tokens: Vec<TokenStream>) {
        self.services.push(ServiceModule {
            module_name,
            tokens,
        });
    }

    pub(super) fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.services
            .into_iter()
            .map(|service| {
                let ServiceModule {
                    module_name,
                    tokens,
                } = service;
                let tokens: TokenStream = quote! {
                    #( #tokens )*
                };

                let rendered = render_tokens(tokens);

                InterfaceArtifact::from_kind(&module_name, InterfaceKind::ExposedService, rendered)
            })
            .collect()
    }
}
