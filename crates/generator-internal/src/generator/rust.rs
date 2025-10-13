use super::templates;
use super::types::{InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage};
use crate::error::Result;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SchemaType, SubscribedAction,
    SubscribedService, SubscribedTopic, TypeToken,
};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
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
    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) {
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let params =
            collect_function_params(topic.message_format.as_ref(), &struct_prefix, &mut context);
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
    }

    fn add_exposed_service(&mut self, service: &ExposedService) {
        let fn_name = prefixed_ident("", non_empty_str(service.name.as_str()), "service");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let params = collect_function_params(
            service.message_format.as_ref(),
            &struct_prefix,
            &mut context,
        );
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
    }

    fn add_exposed_action(&mut self, action: &ExposedAction) {
        let base_ident = prefixed_ident("", non_empty_str(&action.name), "action");
        let base_name = base_ident.to_string();

        let mut context = GenerationContext::default();
        let mut function_blocks: Vec<TokenStream> = Vec::new();

        if let Some(goal) = action.goal_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_goal"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Goal", to_camel_case(&base_name));
            let params =
                collect_function_params(goal.message_format.as_ref(), &struct_prefix, &mut context);

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
            );

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
            );

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
            return;
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
    }

    fn add_subscribed_topic(&mut self, topic: &SubscribedTopic, arguments: Option<&MessageFormat>) {
        let fn_name = Ident::new(topic.callback.as_str(), Span::call_site());
        let (struct_tokens, function_token) = build_async_returning_function(
            &fn_name,
            arguments,
            "Arguments",
            "await for message with PMI",
        );

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
    }

    fn add_subscribed_service(
        &mut self,
        service: &SubscribedService,
        arguments: Option<&MessageFormat>,
    ) {
        let fn_name = Ident::new(service.callback.as_str(), Span::call_site());
        let (struct_tokens, function_token) = build_async_returning_function(
            &fn_name,
            arguments,
            "Arguments",
            "await for service response with PMI",
        );

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
    }

    fn add_subscribed_action(
        &mut self,
        action: &SubscribedAction,
        messages: Option<&SubscribedActionMessage>,
    ) {
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
            );
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
            );
            struct_blocks.extend(struct_tokens);
            function_blocks.push(function_token);
        }

        if function_blocks.is_empty() {
            return;
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
    }

    fn build(self, to_path: impl AsRef<Path>) -> Result<()> {
        let artifacts = self.sections;
        templates::generate_lib_structure(&to_path)?;
        templates::add_artifacts_to_lib(&to_path, artifacts)?;
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

struct FunctionParam {
    ident: Ident,
    ty: TokenStream,
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
) -> Vec<FunctionParam> {
    let mut params = Vec::new();

    if let Some(format) = format {
        for (index, (name, schema)) in format.0.iter().enumerate() {
            let ident = sanitized_ident(name, "param", index);
            let hint = format!("{struct_prefix}{}", to_camel_case(name));
            let ty = rust_type_from_schema(schema, context, &hint);
            params.push(FunctionParam { ident, ty });
        }
    }

    params
}

fn unused_params_stmt(params: &[FunctionParam]) -> TokenStream {
    if params.is_empty() {
        TokenStream::new()
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
) -> (Vec<TokenStream>, TokenStream) {
    let fn_name_str = fn_name.to_string();
    let struct_prefix = to_camel_case(&fn_name_str);
    let mut context = GenerationContext::default();
    let params = collect_function_params(arguments, &struct_prefix, &mut context);

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

    (struct_tokens, function_token)
}

fn rust_type_from_schema(
    schema: &SchemaType,
    context: &mut GenerationContext,
    type_hint: &str,
) -> TokenStream {
    match schema {
        SchemaType::Type(token) => rust_type_for_token(token),
        SchemaType::Array(array) => {
            let element_hint = format!("{type_hint}Item");
            let element_ty = rust_type_from_schema(&array.items, context, &element_hint);
            if let Some(length) = array.length {
                let literal = Literal::usize_unsuffixed(length);
                quote!([#element_ty; #literal])
            } else {
                quote!(Vec<#element_ty>)
            }
        }
        SchemaType::Object(map) => {
            let struct_name = if type_hint.is_empty() {
                "GeneratedType".to_string()
            } else {
                type_hint.to_string()
            };

            let struct_ident = Ident::new(&struct_name, Span::call_site());
            let return_ident = struct_ident.clone();

            let mut fields = Vec::new();
            for (index, (field_name, field_schema)) in map.iter().enumerate() {
                let field_ident = sanitized_ident(field_name, "field", index);
                let nested_hint = format!("{struct_name}{}", to_camel_case(field_name));
                let field_ty = rust_type_from_schema(field_schema, context, &nested_hint);
                fields.push((field_ident, field_ty));
            }

            context.add_struct(struct_ident, fields);
            quote!(#return_ident)
        }
    }
}

fn rust_type_for_token(token: &TypeToken) -> TokenStream {
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

fn sanitized_ident(raw: &str, fallback_prefix: &str, fallback_index: usize) -> Ident {
    let sanitized = sanitize_component(raw);
    let name = if sanitized.is_empty() {
        format!("{fallback_prefix}_{fallback_index}")
    } else {
        sanitized
    };
    Ident::new(&name, Span::call_site())
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

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{
        ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
        SubscribedTopic,
    };
    use std::process::Command;
    use tempfile::TempDir;

    macro_rules! assert_rendered {
        ($cond:expr, $rendered:expr, $($arg:tt)+) => {
            if !$cond {
                eprintln!("rendered output:\n{}", $rendered);
                panic!($($arg)+);
            }
        };
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

    // The user of the lib would use it like this:
    // peppygen::topics::uvc_camera::push_frame(header, encoding, width, height, image);
    #[test]
    fn exposed_topic_gen_calling_code() {
        let topic = r#"
        {
            name: "push_frame", // The name of the topic inside the `uvc_camera` node
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
        let topic: ExposedTopic = serde_json5::from_str(topic).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_topic(&topic);
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated topic code:\n{rendered}");

        assert_rendered!(
            rendered.contains("pub fn push_frame("),
            &rendered,
            "expected sync function"
        );
        assert_rendered!(
            rendered.contains("pub async fn push_frame_async("),
            &rendered,
            "expected async function"
        );
        assert_rendered!(
            rendered.contains("header: PushFrameHeader"),
            &rendered,
            "expected structured header argument"
        );
        assert_rendered!(
            rendered.contains("image: [u8; 3]"),
            &rendered,
            "expected fixed-size array argument"
        );
        assert_rendered!(
            rendered.contains("pub struct PushFrameHeader"),
            &rendered,
            "expected generated struct for nested object"
        );
    }

    #[test]
    fn exposed_service_gen_calling_code() {
        let service = r#"
        {
            name: "enable_camera",
            qos_profile: "critical",
            message_format: {
                enable: "bool"
            }
        }
        "#;
        let service: ExposedService = serde_json5::from_str(service).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_service(&service);
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated service code:\n{rendered}");

        assert_rendered!(
            rendered.contains("pub fn enable_camera("),
            &rendered,
            "expected sync service function"
        );
        assert_rendered!(
            rendered.contains("pub async fn enable_camera_async("),
            &rendered,
            "expected async service function"
        );
        assert_rendered!(
            rendered.contains("enable: bool"),
            &rendered,
            "expected bool argument"
        );
    }

    #[test]
    fn exposed_action_gen_calling_code() {
        let action = r#"
        {
            name: "move_arm",
            goal_service: {
                message_format: {
                    arm_id: "u16",
                    desired_position: {
                        type: "array",
                        items: "i32",
                        length: 3
                    }
                }
            },
            feedback_topic: {
                message_format: {
                    payload: "bytes"
                }
            },
            result_service: {
                message_format: {
                    final_position: {
                        type: "array",
                        items: "i32",
                        length: 3
                    }
                }
            }
        }
        "#;
        let action: ExposedAction = serde_json5::from_str(action).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_action(&action);
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated action code:\n{rendered}");

        for expected in [
            "pub fn move_arm_goal(",
            "pub async fn move_arm_goal_async(",
            "pub fn move_arm_feedback(",
            "pub async fn move_arm_feedback_async(",
            "pub fn move_arm_result(",
            "pub async fn move_arm_result_async(",
        ] {
            assert_rendered!(
                rendered.contains(expected),
                &rendered,
                "expected `{expected}` in rendered"
            );
        }

        assert_rendered!(
            rendered.contains("arm_id: u16"),
            &rendered,
            "expected goal argument"
        );
        assert_rendered!(
            rendered.contains("desired_position: [i32; 3]"),
            &rendered,
            "expected goal array argument"
        );
        assert_rendered!(
            rendered.contains("payload: Vec<u8>"),
            &rendered,
            "expected feedback payload argument"
        );
        assert_rendered!(
            rendered.contains("final_position: [i32; 3]"),
            &rendered,
            "expected result array argument"
        );
    }

    #[test]
    fn subscribed_topic_returns_arguments() {
        let topic = r#"
        {
            node: "uvc_camera",
            name: "stream",
            tag: "0.1.0",
            callback: "on_video_frame_received"
        }
        "#;
        let topic: SubscribedTopic = serde_json5::from_str(topic).unwrap();
        let format = r#"
        {
            header: {
                stamp: "time",
                frame_id: "u32"
            },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: {
                type: "array",
                items: "u8",
                length: 3
            }
        }
        "#;
        let format: MessageFormat = serde_json5::from_str(format).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_subscribed_topic(&topic, Some(&format));
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated subscribed topic code:\n{rendered}");

        assert_rendered!(
            rendered.contains(
                "pub async fn on_video_frame_received() -> OnVideoFrameReceivedArguments"
            ),
            &rendered,
            "expected async subscriber function with return type"
        );
        assert_rendered!(
            rendered.contains("pub struct OnVideoFrameReceivedArguments"),
            &rendered,
            "expected return struct definition"
        );
        assert_rendered!(
            rendered.contains("pub struct OnVideoFrameReceivedHeader"),
            &rendered,
            "expected nested struct definition"
        );
        assert_rendered!(
            rendered.contains("image: [u8; 3]"),
            &rendered,
            "expected array element type"
        );
    }

    #[test]
    fn subscribed_service_returns_arguments() {
        let service = r#"
        {
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0",
            callback: "on_get_camera_info"
        }
        "#;
        let service: SubscribedService = serde_json5::from_str(service).unwrap();
        let format = r#"
        {
            card_type: "string",
            size: "string",
            interval: "string"
        }
        "#;
        let format: MessageFormat = serde_json5::from_str(format).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_subscribed_service(&service, Some(&format));
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated subscribed service code:\n{rendered}");

        assert_rendered!(
            rendered.contains("pub async fn on_get_camera_info() -> OnGetCameraInfoArguments"),
            &rendered,
            "expected async service subscriber"
        );
        assert_rendered!(
            rendered.contains("pub struct OnGetCameraInfoArguments"),
            &rendered,
            "expected return struct"
        );
        assert_rendered!(
            rendered.contains("card_type: String"),
            &rendered,
            "expected field mapping"
        );
    }

    #[test]
    fn subscribed_action_returns_arguments() {
        let action = r#"
        {
            node: "brain",
            name: "move_arm",
            tag: "0.1.0",
            feedback_callback: "on_move_arm_feedback",
            results_callback: "on_move_arm_result"
        }
        "#;
        let action: SubscribedAction = serde_json5::from_str(action).unwrap();
        let goal_format: MessageFormat = serde_json5::from_str(
            r#"{
            arm_id: "u16",
            desired_position: {
                type: "array",
                items: "i32",
                length: 3
            }
        }"#,
        )
        .unwrap();
        let feedback_format: MessageFormat =
            serde_json5::from_str(r#"{ payload: "bytes" }"#).unwrap();
        let result_format: MessageFormat = serde_json5::from_str(
            r#"{
            final_position: {
                type: "array",
                items: "i32",
                length: 3
            }
        }"#,
        )
        .unwrap();
        let format = SubscribedActionMessage {
            goal: goal_format,
            feedback: feedback_format,
            result: result_format,
        };

        let mut generator = RustGenerator::new();
        generator.add_subscribed_action(&action, Some(&format));
        let artifacts: Vec<String> = generator
            .into_artifacts()
            .into_iter()
            .map(|artifact| artifact.code_output)
            .collect();
        let rendered = single_artifact(artifacts);

        println!("generated subscribed action code:\n{rendered}");

        assert!(
            !rendered.contains("pub async fn on_move_arm_goal()"),
            "unexpected goal callback generated:\n{rendered}"
        );

        for expected in [
            "pub async fn on_move_arm_feedback() -> OnMoveArmFeedbackArguments",
            "pub async fn on_move_arm_result() -> OnMoveArmResultArguments",
        ] {
            assert_rendered!(
                rendered.contains(expected),
                &rendered,
                "expected `{expected}` in rendered"
            );
        }

        assert_rendered!(
            rendered.contains("payload: Vec<u8>"),
            &rendered,
            "expected feedback payload"
        );
        assert_rendered!(
            rendered.contains("final_position: [i32; 3]"),
            &rendered,
            "expected result array"
        );
        assert!(
            !rendered.contains("arm_id: u16"),
            "goal fields should not appear in feedback or result structs:\n{rendered}"
        );
    }

    #[test]
    fn create_lib_basic_structure() {
        let temp_dir = TempDir::new().unwrap();
        let generator = RustGenerator::new();
        generator.build(&temp_dir).unwrap();

        assert!(
            temp_dir.path().join("Cargo.toml").exists(),
            "Expected Cargo.toml to be generated in the temporary crate directory"
        );

        let output = Command::new("cargo")
            .arg("build")
            .current_dir(temp_dir.path())
            .output()
            .expect("failed to invoke cargo build on generated crate");
        let status = output.status;

        assert!(
            status.success(),
            "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
            status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn compile_peppygen_with_multiple_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        let topic = r#"
        {
            name: "push_frame", // The name of the topic inside the `uvc_camera` node
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
        let service = r#"
        {
          name: "enable_camera",
          qos_profile: "critical",
          message_format: {
            enable: "bool",
          }
        }
        "#;
        let topic: ExposedTopic = serde_json5::from_str(topic).unwrap();
        let service: ExposedService = serde_json5::from_str(service).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_topic(&topic);
        generator.add_exposed_service(&service);
        generator.build(&temp_dir).unwrap();

        assert!(
            temp_dir.path().join("Cargo.toml").exists(),
            "Expected Cargo.toml to be generated in the temporary crate directory"
        );

        let output = Command::new("cargo")
            .arg("build")
            .current_dir(temp_dir.path())
            .output()
            .expect("failed to invoke cargo build on generated crate");
        let status = output.status;

        assert!(
            status.success(),
            "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
            status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn create_lib_with_exposed_topic_artifact() {
        let temp_dir = TempDir::new().unwrap();
        let topic = r#"
        {
            name: "push_frame", // The name of the topic inside the `uvc_camera` node
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
        let topic: ExposedTopic = serde_json5::from_str(topic).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_topic(&topic);
        generator.build(&temp_dir).unwrap();

        assert!(
            temp_dir.path().join("Cargo.toml").exists(),
            "Expected Cargo.toml to be generated in the temporary crate directory"
        );

        let lib_rs = temp_dir.path().join("src/lib.rs");
        assert!(
            lib_rs.exists(),
            "Expected lib.rs to exist so `peppygen::topics` is reachable"
        );
        let lib_contents =
            std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
        assert!(
            lib_contents.contains("pub mod topics;"),
            "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
            lib_contents
        );

        let topics_mod = temp_dir.path().join("src/topics.rs");
        assert!(
            topics_mod.exists(),
            "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
        );
        let topics_contents =
            std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
        assert!(
            topics_contents.contains("pub mod push_frame;"),
            "Expected topics module to expose generated `push_frame` module, got:\n{}",
            topics_contents
        );

        let push_frame_mod = temp_dir.path().join("src/topics/push_frame.rs");
        assert!(
            push_frame_mod.exists(),
            "Expected generated topic module at {:?}",
            push_frame_mod
        );
        let push_frame_contents =
            std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
        assert!(
            push_frame_contents.contains("pub fn push_frame("),
            "Expected generated module to expose sync topic function, got:\n{}",
            push_frame_contents
        );
        assert!(
            push_frame_contents.contains("pub async fn push_frame_async("),
            "Expected generated module to expose async topic function, got:\n{}",
            push_frame_contents
        );
    }

    #[test]
    fn create_lib_with_exposed_double_topic_artifact() {
        let temp_dir = TempDir::new().unwrap();
        let topic = r#"
        {
            name: "push_frame", // The name of the topic inside the `uvc_camera` node
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

        let topic2 = r#"
        {
            name: "push_lidar_object", // The name of the topic inside the `lidar_sensor` node
            qos_profile: "sensor_data",
            message_format: {
                header: {
                  stamp: "time",
                  frame_id: "u32",
                },
                x: "f32",
                y: "f32",
                z: "f32",
                intensity: "f32",
                return_type: "u8", // e.g. first return, last return
                classification: "u8", // type of object detected
            },
        }
        "#;
        let topic: ExposedTopic = serde_json5::from_str(topic).unwrap();
        let topic2: ExposedTopic = serde_json5::from_str(topic2).unwrap();

        let mut generator = RustGenerator::new();
        generator.add_exposed_topic(&topic);
        generator.add_exposed_topic(&topic2);
        generator.build(&temp_dir).unwrap();

        assert!(
            temp_dir.path().join("Cargo.toml").exists(),
            "Expected Cargo.toml to be generated in the temporary crate directory"
        );

        let lib_rs = temp_dir.path().join("src/lib.rs");
        assert!(
            lib_rs.exists(),
            "Expected lib.rs to exist so `peppygen::topics` is reachable"
        );
        let lib_contents =
            std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
        assert!(
            lib_contents.contains("pub mod topics;"),
            "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
            lib_contents
        );

        let topics_mod = temp_dir.path().join("src/topics.rs");
        assert!(
            topics_mod.exists(),
            "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
        );
        let topics_contents =
            std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
        assert!(
            topics_contents.contains("pub mod push_frame;"),
            "Expected topics module to expose generated `push_frame` module, got:\n{}",
            topics_contents
        );
        assert!(
            topics_contents.contains("pub mod push_lidar_object;"),
            "Expected topics module to expose generated `push_frame` module, got:\n{}",
            topics_contents
        );

        let push_frame_mod = temp_dir.path().join("src/topics/push_frame.rs");
        assert!(
            push_frame_mod.exists(),
            "Expected generated topic module at {:?}",
            push_frame_mod
        );
        let push_frame_contents =
            std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
        assert!(
            push_frame_contents.contains("pub fn push_frame("),
            "Expected generated module to expose sync topic function, got:\n{}",
            push_frame_contents
        );
        assert!(
            push_frame_contents.contains("pub async fn push_frame_async("),
            "Expected generated module to expose async topic function, got:\n{}",
            push_frame_contents
        );

        let push_lidar_mod = temp_dir.path().join("src/topics/push_lidar_object.rs");
        assert!(
            push_lidar_mod.exists(),
            "Expected generated topic module at {:?}",
            push_lidar_mod
        );
        let push_lidar_contents = std::fs::read_to_string(&push_lidar_mod)
            .expect("failed to read push_lidar_object module");
        assert!(
            push_lidar_contents.contains("pub fn push_lidar_object("),
            "Expected generated module to expose sync topic function, got:\n{}",
            push_lidar_contents
        );
        assert!(
            push_lidar_contents.contains("pub async fn push_lidar_object_async("),
            "Expected generated module to expose async topic function, got:\n{}",
            push_lidar_contents
        );
    }

    // TODO: Create tests similar to create_lib_with_exposed_topic_artifact but with services/actions and a combination of the 3
}
