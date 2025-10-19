#[cfg(test)]
mod tests;

use super::checker;
use super::common;
use super::types::{InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage};
use crate::error::Result;
use config::encoding::{FunctionParam, map_message_format_to_capnpn_proto};
use config::{
    consts::PEPPY_NODE_CONFIG_FILE,
    node::{
        ExposedAction, ExposedService, ExposedTopic, MessageFormat, SchemaType, SubscribedAction,
        SubscribedService, SubscribedTopic, TypeToken,
    },
};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::HashMap;
use std::path::Path;
use syn::{File, parse2};

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

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }
}

impl LanguageGenerator for RustGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) -> Result<()> {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
        Ok(())
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let params =
            collect_function_params(topic.message_format.as_ref(), &struct_prefix, &mut context)?;
        let struct_tokens = context.into_tokens();

        let sync_fn =
            build_sync_function(&fn_name, &params, "publish peppylib topic synchronously");
        let async_fn = build_async_function(
            &async_fn_name,
            &params,
            "publish peppylib topic asynchronously",
        );
        let function_tokens = quote! {
            #sync_fn
            #async_fn
        };

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Topics {
                #function_tokens
            }
        };
        let rendered = render_tokens(tokens);

        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::ExposedTopic,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_service(&mut self, service: &ExposedService) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let params = collect_function_params(
            service.message_format.as_ref(),
            &struct_prefix,
            &mut context,
        )?;
        let struct_tokens = context.into_tokens();

        let sync_fn = build_sync_function(
            &fn_name,
            &params,
            "call peppylib and send message service synchronously",
        );
        let async_fn = build_async_function(
            &async_fn_name,
            &params,
            "call peppylib and send message service asynchronously",
        );
        let function_tokens = quote! {
            #sync_fn
            #async_fn
        };

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Services {
                #function_tokens
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::ExposedService,
            rendered,
        ));
        Ok(())
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) -> Result<()> {
        let base_ident = prefixed_ident("", non_empty_str(&action.name), "action");
        let base_name = base_ident.to_string();

        let mut context = GenerationContext::default();
        let mut function_blocks: Vec<TokenStream> = Vec::new();

        if let Some(goal) = action.goal_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_goal"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Goal", to_camel_case(&base_name));
            let params = collect_function_params(
                goal.message_format.as_ref(),
                &struct_prefix,
                &mut context,
            )?;

            let sync_fn = build_sync_function(
                &fn_name,
                &params,
                "call peppylib and send message action goal synchronously",
            );
            let async_fn = build_async_function(
                &async_fn_name,
                &params,
                "call peppylib and send message action goal asynchronously",
            );
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(feedback) = action.feedback_topic.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_feedback"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Feedback", to_camel_case(&base_name));
            let params = collect_function_params(
                feedback.message_format.as_ref(),
                &struct_prefix,
                &mut context,
            )?;

            let sync_fn = build_sync_function(
                &fn_name,
                &params,
                "publish PMI action feedback synchronously",
            );
            let async_fn = build_async_function(
                &async_fn_name,
                &params,
                "publish PMI action feedback asynchronously",
            );
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(result) = action.result_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_result"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Result", to_camel_case(&base_name));
            let params = collect_function_params(
                result.message_format.as_ref(),
                &struct_prefix,
                &mut context,
            )?;

            let sync_fn = build_sync_function(
                &fn_name,
                &params,
                "call peppylib and send message action result synchronously",
            );
            let async_fn = build_async_function(
                &async_fn_name,
                &params,
                "call peppylib and send message action result asynchronously",
            );
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if function_blocks.is_empty() {
            return Ok(());
        }

        let struct_tokens = context.into_tokens();

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Actions {
                #( #function_blocks )*
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::ExposedAction,
            rendered,
        ));
        Ok(())
    }

    fn add_subscribed_topic(
        &mut self,
        topic: &SubscribedTopic,
        arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        let fn_name = Ident::new(topic.callback.as_str(), Span::call_site());
        let (struct_tokens, function_token) = build_async_returning_function(
            &fn_name,
            arguments,
            "Arguments",
            "await for message with PMI",
        )?;

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Topics {
                #function_token
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &topic.name,
            InterfaceKind::SubscribedTopic,
            rendered,
        ));
        Ok(())
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    ) -> Result<()> {
        let fn_name = Ident::new(service.callback.as_str(), Span::call_site());
        let (struct_tokens, function_token) = build_async_returning_function(
            &fn_name,
            arguments,
            "Arguments",
            "await for service response with PMI",
        )?;

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Services {
                #function_token
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &service.name,
            InterfaceKind::SubscribedService,
            rendered,
        ));
        Ok(())
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: Option<&SubscribedActionMessage>,
    ) -> Result<()> {
        let mut struct_blocks: Vec<TokenStream> = Vec::new();
        let mut function_blocks: Vec<TokenStream> = Vec::new();

        if let Some(callback) = action.feedback_callback.as_ref() {
            let fn_name = Ident::new(callback.as_str(), Span::call_site());
            let feedback_format = messages.map(|msg| &msg.feedback);
            let (struct_tokens, function_token) = build_async_returning_function(
                &fn_name,
                feedback_format,
                "Arguments",
                "await for action feedback with PMI",
            )?;
            struct_blocks.extend(struct_tokens);
            function_blocks.push(function_token);
        }

        if let Some(callback) = action.results_callback.as_ref() {
            let fn_name = Ident::new(callback.as_str(), Span::call_site());
            let result_format = messages.map(|msg| &msg.result);
            let (struct_tokens, function_token) = build_async_returning_function(
                &fn_name,
                result_format,
                "Arguments",
                "await for action result with PMI",
            )?;
            struct_blocks.extend(struct_tokens);
            function_blocks.push(function_token);
        }

        if function_blocks.is_empty() {
            return Ok(());
        }

        let tokens: TokenStream = quote! {
            #( #struct_blocks )*

            impl Actions {
                #( #function_blocks )*
            }
        };
        let rendered = render_tokens(tokens);
        self.push_section(InterfaceArtifact::from_kind(
            &action.name,
            InterfaceKind::SubscribedAction,
            rendered,
        ));
        Ok(())
    }

    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let artifacts = self.sections;
        common::add_peppylib_dependencies(&to_path)?;
        common::add_artifacts_to_lib(&to_path, artifacts)?;
        let crate_root = to_path.as_ref();
        let node_config_path = crate_root.join(PEPPY_NODE_CONFIG_FILE);
        // Lastly generate the codegen fingerprint based on the peppy.json5 config file
        checker::generate_node_config_fingerprint(&node_config_path, crate_root)?;
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

#[derive(Default)]
struct GenerationContext {
    structs: Vec<StructDefinition>,
}

impl GenerationContext {
    fn add_struct(&mut self, ident: Ident, fields: Vec<(Ident, TokenStream)>) {
        if let Some(existing) = self.structs.iter_mut().find(|def| def.ident == ident) {
            *existing = StructDefinition { ident, fields };
        } else {
            self.structs.push(StructDefinition { ident, fields });
        }
    }

    fn into_tokens(self) -> Vec<TokenStream> {
        self.structs
            .into_iter()
            .map(StructDefinition::into_tokens)
            .collect()
    }
}

struct StructDefinition {
    ident: Ident,
    fields: Vec<(Ident, TokenStream)>,
}

impl StructDefinition {
    fn into_tokens(self) -> TokenStream {
        let ident = self.ident;
        let field_tokens: Vec<TokenStream> = self
            .fields
            .into_iter()
            .map(|(field_ident, ty)| {
                let name = field_ident;
                let field_ty = ty;
                quote!(pub #name: #field_ty)
            })
            .collect();

        if field_tokens.is_empty() {
            quote! {
                #[derive(Debug, Clone)]
                pub struct #ident {}
            }
        } else {
            quote! {
                #[derive(Debug, Clone)]
                pub struct #ident {
                    #( #field_tokens ),*
                }
            }
        }
    }
}

fn collect_function_params(
    format: Option<&MessageFormat>,
    struct_prefix: &str,
    context: &mut GenerationContext,
) -> Result<Vec<FunctionParam>> {
    let Some(format) = format else {
        return Ok(Vec::new());
    };

    let (_, capnp_params) = map_message_format_to_capnpn_proto(format.clone()).map_err(|err| {
        crate::error::Error::from(std::io::Error::new(
            std::io::ErrorKind::Other,
            err.to_string(),
        ))
    })?;

    let mut schema_lookup: HashMap<String, (&String, &SchemaType)> =
        HashMap::with_capacity(format.0.len());
    for (name, schema) in &format.0 {
        schema_lookup.insert(sanitize_capnp_field_name(name), (name, schema));
    }

    let mut params = Vec::with_capacity(capnp_params.len());
    for param in capnp_params {
        let key = param.ident.to_string();
        let (original_name, schema) = schema_lookup
            .get(&key)
            .unwrap_or_else(|| panic!("missing schema entry for field `{key}`"));

        let ident = Ident::new(&sanitize_component(original_name), Span::call_site());
        let ty = schema_type_to_tokens(schema, struct_prefix, original_name, context);

        params.push(FunctionParam::new(ident, ty));
    }

    Ok(params)
}

fn schema_type_to_tokens(
    schema: &SchemaType,
    struct_prefix: &str,
    field_name: &str,
    context: &mut GenerationContext,
) -> TokenStream {
    match schema {
        SchemaType::Type(token) => primitive_type_token(token),
        SchemaType::Array(array) => {
            let item_ty = match array.items.as_ref() {
                SchemaType::Type(token) => primitive_type_token(token),
                other => panic!(
                    "unsupported nested schema type {:?} in array `{field_name}`",
                    other
                ),
            };

            if let Some(length) = array.length {
                let len_lit = Literal::usize_unsuffixed(length);
                quote!([#item_ty; #len_lit])
            } else {
                quote!(Vec<#item_ty>)
            }
        }
        SchemaType::Object(object) => {
            let struct_name = format!("{struct_prefix}{}", to_camel_case(field_name));
            let struct_ident = Ident::new(&struct_name, Span::call_site());

            let mut fields = Vec::with_capacity(object.fields.len());
            for (nested_name, nested_schema) in &object.fields {
                let field_ident =
                    Ident::new(&sanitize_component(nested_name.as_str()), Span::call_site());
                let field_ty =
                    schema_type_to_tokens(nested_schema, &struct_name, nested_name, context);
                fields.push((field_ident, field_ty));
            }

            context.add_struct(struct_ident.clone(), fields);
            quote!(#struct_ident)
        }
    }
}

fn primitive_type_token(token: &TypeToken) -> TokenStream {
    match token {
        TypeToken::Bool => quote!(bool),
        TypeToken::String => quote!(String),
        TypeToken::Bytes => quote!(Vec<u8>),
        TypeToken::Time => quote!(std::time::SystemTime),
        TypeToken::U8 => quote!(u8),
        TypeToken::U16 => quote!(u16),
        TypeToken::U32 => quote!(u32),
        TypeToken::U64 => quote!(u64),
        TypeToken::I8 => quote!(i8),
        TypeToken::I16 => quote!(i16),
        TypeToken::I32 => quote!(i32),
        TypeToken::I64 => quote!(i64),
        TypeToken::F32 => quote!(f32),
        TypeToken::F64 => quote!(f64),
    }
}

fn sanitize_capnp_field_name(input: &str) -> String {
    fn to_pascal_case(input: &str) -> String {
        let mut result = String::new();

        for segment in input
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
        {
            let mut chars = segment.chars();
            if let Some(first) = chars.next() {
                result.push(first.to_ascii_uppercase());
                for ch in chars {
                    result.push(ch.to_ascii_lowercase());
                }
            }
        }

        result
    }

    let mut output = to_pascal_case(input);
    if output.is_empty() {
        output.push_str("Field");
    }

    let mut chars = output.chars();
    let mut camel = String::with_capacity(output.len());
    if let Some(first) = chars.next() {
        camel.push(first.to_ascii_lowercase());
        camel.extend(chars);
    }

    if camel.chars().next().map_or(false, |ch| ch.is_ascii_digit()) {
        camel.insert(0, '_');
    }

    if camel.is_empty() {
        "_field".to_string()
    } else {
        camel
    }
}

fn unused_params_stmt(params: &[FunctionParam]) -> TokenStream {
    if params.is_empty() {
        TokenStream::new()
    } else if params.len() == 1 {
        let ident = &params[0].ident;
        quote! {
            let _ = &#ident;
        }
    } else {
        let refs: Vec<TokenStream> = params
            .iter()
            .map(|param| {
                let ident = &param.ident;
                quote!(&#ident)
            })
            .collect();
        quote! {
            let _ = (#(#refs),*);
        }
    }
}

fn render_tokens(tokens: TokenStream) -> String {
    parse2::<File>(tokens.clone())
        .map(|file| prettyplease::unparse(&file))
        .unwrap_or_else(|_| tokens.to_string())
}

fn build_sync_function(fn_name: &Ident, params: &[FunctionParam], todo_msg: &str) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();

    let suppress_unused = unused_params_stmt(params);
    let msg = Literal::string(todo_msg);

    quote! {
        pub fn #fn_name(#(#param_tokens),*) {
            #suppress_unused
            todo!(#msg);
        }
    }
}

fn build_async_function(fn_name: &Ident, params: &[FunctionParam], todo_msg: &str) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();

    let suppress_unused = unused_params_stmt(params);
    let msg = Literal::string(todo_msg);

    quote! {
        pub async fn #fn_name(#(#param_tokens),*) {
            #suppress_unused
            todo!(#msg);
        }
    }
}

fn build_async_returning_function(
    fn_name: &Ident,
    arguments: Option<&MessageFormat>,
    struct_suffix: &str,
    todo_msg: &str,
) -> Result<(Vec<TokenStream>, TokenStream)> {
    let fn_name_str = fn_name.to_string();
    let struct_prefix = to_camel_case(&fn_name_str);
    let mut context = GenerationContext::default();
    let params = collect_function_params(arguments, &struct_prefix, &mut context)?;

    let struct_name = if struct_suffix.is_empty() {
        struct_prefix
    } else {
        format!("{struct_prefix}{struct_suffix}")
    };
    let struct_ident = Ident::new(&struct_name, Span::call_site());

    let fields: Vec<(Ident, TokenStream)> = params
        .iter()
        .map(|param| (param.ident.clone(), param.ty.clone()))
        .collect();
    context.add_struct(struct_ident.clone(), fields);

    let struct_tokens = context.into_tokens();
    let todo_literal = Literal::string(todo_msg);

    let function_token = quote! {
        pub async fn #fn_name() -> #struct_ident {
            todo!(#todo_literal);
        }
    };

    Ok((struct_tokens, function_token))
}

fn to_camel_case(raw: &str) -> String {
    let sanitized = sanitize_component(raw);
    let mut out = String::new();

    for segment in sanitized.split('_').filter(|segment| !segment.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }

    if out.is_empty() {
        out.push_str("Item");
    }

    if !out
        .chars()
        .next()
        .map(|ch| ch.is_ascii_alphabetic())
        .unwrap_or(true)
    {
        out.insert(0, 'T');
    }

    out
}
