#[cfg(test)]
mod tests;

use super::checker;
use super::common;
use super::types::{
    CapnpSchema, InterfaceArtifact, InterfaceKind, LanguageGenerator, SubscribedActionMessage,
};
use crate::error::Error;
use crate::error::Result;
use config::encoding::{CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper};
use config::{
    consts::PEPPY_NODE_CONFIG_FILE,
    node::{
        ExposedAction, ExposedService, ExposedTopic, MessageFormat, SchemaType, SubscribedAction,
        SubscribedService, SubscribedTopic, TypeToken,
    },
};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use syn::{File, parse2};

/// Rust-specific implementation of the interface generator.
pub struct RustGenerator {
    sections: Vec<InterfaceArtifact>,
    schemas: HashMap<String, CapnpSchema>,
}

impl RustGenerator {
    pub fn new() -> Self {
        Self {
            sections: Vec::new(),
            schemas: HashMap::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn into_artifacts(self) -> Vec<InterfaceArtifact> {
        self.sections
    }

    fn register_schema(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: &CapnpSchemaArtifacts,
    ) -> Result<SchemaInfo> {
        let schema_source = artifacts.encoding_schema();

        let key_component = sanitize_component(schema_key);
        let base_name = if key_component.is_empty() {
            "message".to_string()
        } else {
            key_component
        };

        let file_stem = if base_name.ends_with("_message") {
            base_name
        } else {
            format!("{base_name}_message")
        };

        let struct_name = format!("{struct_prefix}Message");
        let schema = schema_source.replacen("struct Message", &format!("struct {struct_name}"), 1);

        let capnp_schema = CapnpSchema::new(file_stem.clone(), schema);
        self.schemas.insert(file_stem.clone(), capnp_schema);

        Ok(SchemaInfo { file_stem })
    }

    fn prepare_message_encoding(
        &mut self,
        schema_key: &str,
        struct_prefix: &str,
        artifacts: Option<&CapnpSchemaArtifacts>,
        params: &[FunctionParam],
    ) -> Result<Option<MessageEncodingSpec>> {
        let Some(artifacts) = artifacts else {
            return Ok(None);
        };

        let schema_info = self.register_schema(schema_key, struct_prefix, artifacts)?;
        let builder_ident = Ident::new("root", Span::call_site());
        let assignments =
            generate_assignments_for_format(&builder_ident, artifacts.message_format(), params);

        Ok(Some(MessageEncodingSpec {
            builder_type: schema_info.builder_type_tokens(),
            assignments,
        }))
    }
}

struct SchemaInfo {
    file_stem: String,
}

impl SchemaInfo {
    fn builder_type_tokens(&self) -> TokenStream {
        let module_ident = Ident::new(&format!("{}_capnp", self.file_stem), Span::call_site());
        let struct_module_ident = Ident::new(&self.file_stem, Span::call_site());
        quote!(crate::capnp::#module_ident::#struct_module_ident::Builder)
    }
}

struct MessageEncodingSpec {
    builder_type: TokenStream,
    assignments: Vec<TokenStream>,
}

impl LanguageGenerator for RustGenerator {
    fn push_section(&mut self, section: InterfaceArtifact) {
        if !section.code_output.is_empty() {
            self.sections.push(section);
        }
    }

    fn add_exposed_topic(&mut self, topic: &ExposedTopic) -> Result<()> {
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let format_artifacts = map_message_format(topic.message_format.as_ref())?;
        let params =
            collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &struct_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();

        let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_name_str);
        let async_fn =
            build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_name_str);
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
        let format_artifacts = map_message_format(service.message_format.as_ref())?;
        let params =
            collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
        let encoding = self.prepare_message_encoding(
            &fn_name_str,
            &struct_prefix,
            format_artifacts.as_ref(),
            &params,
        )?;
        let struct_tokens = context.into_tokens();

        let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_name_str);
        let async_fn =
            build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_name_str);
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
            let format_artifacts = map_message_format(goal.message_format.as_ref())?;
            let params =
                collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(feedback) = action.feedback_topic.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_feedback"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Feedback", to_camel_case(&base_name));
            let format_artifacts = map_message_format(feedback.message_format.as_ref())?;
            let params =
                collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
            function_blocks.push(sync_fn);
            function_blocks.push(async_fn);
        }

        if let Some(result) = action.result_service.as_ref() {
            let fn_name = Ident::new(&(base_name.clone() + "_result"), Span::call_site());
            let async_fn_name = Ident::new(&(fn_name.to_string() + "_async"), Span::call_site());
            let struct_prefix = format!("{}Result", to_camel_case(&base_name));
            let format_artifacts = map_message_format(result.message_format.as_ref())?;
            let params =
                collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;
            let fn_key = fn_name.to_string();
            let encoding = self.prepare_message_encoding(
                &fn_key,
                &struct_prefix,
                format_artifacts.as_ref(),
                &params,
            )?;

            let sync_fn = build_sync_function(&fn_name, &async_fn_name, &params, &fn_key);
            let async_fn =
                build_async_function(&async_fn_name, &params, encoding.as_ref(), &fn_key);
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
        // First create the basic structure of the project
        common::add_peppylib_dependencies(&to_path)?;
        // Write the schema files to the project
        common::write_capnp_schemas(&self.schemas, to_path.as_ref())?;
        // Add the content to the Rust files
        common::add_artifacts_to_lib(&to_path, self.sections)?;

        // TODO: The services should start when the node starts, there should be a way to pass in the callbacks for the service to start in peppygen
        // add_startup_services();

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

fn map_message_format(format: Option<&MessageFormat>) -> Result<Option<CapnpSchemaArtifacts>> {
    match format {
        Some(format) => MessageFormatMapper::new(format.clone())
            .map_message_format_to_capnpn()
            .map(Some)
            .map_err(Error::MessageEncoding),
        None => Ok(None),
    }
}

fn collect_function_params(
    artifacts: Option<&CapnpSchemaArtifacts>,
    struct_prefix: &str,
    context: &mut GenerationContext,
) -> Result<Vec<FunctionParam>> {
    let Some(artifacts) = artifacts else {
        return Ok(Vec::new());
    };

    let format = artifacts.message_format();
    let capnp_params = artifacts
        .build_function_params()
        .map_err(Error::MessageEncoding)?;

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

fn generate_assignments_for_format(
    builder_ident: &Ident,
    format: &MessageFormat,
    params: &[FunctionParam],
) -> Vec<TokenStream> {
    let mut param_lookup: HashMap<String, Ident> = HashMap::new();
    for param in params {
        param_lookup.insert(param.ident.to_string(), param.ident.clone());
    }

    let mut assignments = Vec::with_capacity(format.0.len());
    let mut name_gen = NameGenerator::new();
    let builder_expr = quote!(#builder_ident);

    for (field_name, schema) in &format.0 {
        let sanitized = sanitize_component(field_name);
        let param_ident = param_lookup
            .get(&sanitized)
            .unwrap_or_else(|| panic!("missing parameter for field `{field_name}`"))
            .clone();
        let value_expr = quote!(#param_ident);
        assignments.push(generate_field_assignment(
            &builder_expr,
            field_name,
            schema,
            &value_expr,
            &mut name_gen,
        ));
    }

    assignments
}

fn generate_field_assignment(
    builder_expr: &TokenStream,
    field_name: &str,
    schema: &SchemaType,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> TokenStream {
    let method_component = sanitize_component(field_name);
    let set_method = Ident::new(&format!("set_{method_component}"), Span::call_site());
    let init_method = Ident::new(&format!("init_{method_component}"), Span::call_site());

    match schema {
        SchemaType::Type(token) => match token {
            TypeToken::Bool
            | TypeToken::U8
            | TypeToken::U16
            | TypeToken::U32
            | TypeToken::U64
            | TypeToken::I8
            | TypeToken::I16
            | TypeToken::I32
            | TypeToken::I64
            | TypeToken::F32
            | TypeToken::F64 => {
                quote!(#builder_expr.#set_method(#value_expr);)
            }
            TypeToken::String => {
                quote!(#builder_expr.#set_method(#value_expr.as_str());)
            }
            TypeToken::Bytes => {
                quote!(#builder_expr.#set_method(#value_expr.as_ref());)
            }
            TypeToken::Time => {
                generate_time_assignment(builder_expr, &init_method, value_expr, names)
            }
        },
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Type(TypeToken::U8) => {
                quote!(#builder_expr.#set_method(#value_expr.as_ref());)
            }
            SchemaType::Type(token) => generate_list_assignment(
                builder_expr,
                &init_method,
                value_expr,
                array.length,
                token,
                names,
            ),
            other => panic!(
                "unsupported nested schema type {:?} in array `{field_name}`",
                other
            ),
        },
        SchemaType::Object(object) => generate_object_assignment(
            builder_expr,
            &init_method,
            value_expr,
            &object.fields,
            names,
        ),
    }
}

fn generate_time_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    names: &mut NameGenerator,
) -> TokenStream {
    let seconds_ident = names.next("seconds");
    let nanos_ident = names.next("nanos");
    let builder_ident = names.next("timestamp_builder");

    quote! {
        let (#seconds_ident, #nanos_ident) = match #value_expr.duration_since(::std::time::UNIX_EPOCH) {
            Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
            Err(err) => {
                let duration = err.duration();
                let secs = duration.as_secs() as i64;
                let nanos = duration.subsec_nanos();
                if nanos == 0 {
                    (-secs, 0)
                } else {
                    (-secs - 1, 1_000_000_000u32 - nanos)
                }
            }
        };
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #builder_ident.set_sec(#seconds_ident);
        #builder_ident.set_nsec(#nanos_ident);
    }
}

fn generate_list_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    length: Option<usize>,
    token: &TypeToken,
    names: &mut NameGenerator,
) -> TokenStream {
    let list_ident = names.next("list");
    let idx_ident = names.next("idx");
    let element_ident = names.next("value");

    let length_expr = match length {
        Some(len) => {
            let len_lit = Literal::u32_unsuffixed(len as u32);
            quote!(#len_lit)
        }
        None => quote!((#value_expr).len() as u32),
    };

    let element_setter = match token {
        TypeToken::String => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_str());),
        TypeToken::Bytes => quote!(#list_ident.set(#idx_ident as u32, #element_ident.as_ref());),
        TypeToken::Bool
        | TypeToken::U8
        | TypeToken::U16
        | TypeToken::U32
        | TypeToken::U64
        | TypeToken::I8
        | TypeToken::I16
        | TypeToken::I32
        | TypeToken::I64
        | TypeToken::F32
        | TypeToken::F64 => quote!(#list_ident.set(#idx_ident as u32, *#element_ident);),
        TypeToken::Time => panic!("time arrays are not supported"),
    };

    quote! {
        let mut #list_ident = #builder_expr.reborrow().#init_method(#length_expr);
        for (#idx_ident, #element_ident) in (#value_expr).iter().enumerate() {
            #element_setter
        }
    }
}

fn generate_object_assignment(
    builder_expr: &TokenStream,
    init_method: &Ident,
    value_expr: &TokenStream,
    fields: &BTreeMap<String, SchemaType>,
    names: &mut NameGenerator,
) -> TokenStream {
    let builder_ident = names.next("builder");
    let mut nested = Vec::with_capacity(fields.len());

    for (nested_name, nested_schema) in fields {
        let nested_ident = Ident::new(&sanitize_component(nested_name.as_str()), Span::call_site());
        let nested_value_expr = quote!(#value_expr.#nested_ident);
        nested.push(generate_field_assignment(
            &quote!(#builder_ident),
            nested_name,
            nested_schema,
            &nested_value_expr,
            names,
        ));
    }

    quote! {
        let mut #builder_ident = #builder_expr.reborrow().#init_method();
        #(#nested)*
    }
}

#[derive(Default)]
struct NameGenerator {
    counter: usize,
}

impl NameGenerator {
    fn new() -> Self {
        Self { counter: 0 }
    }

    fn next(&mut self, hint: &str) -> Ident {
        let sanitized = sanitize_component(hint);
        let suffix = self.counter;
        self.counter += 1;
        let base = if sanitized.is_empty() {
            "tmp".to_string()
        } else {
            sanitized
        };
        Ident::new(&format!("{base}_{suffix}"), Span::call_site())
    }
}

fn build_sync_function(
    fn_name: &Ident,
    async_fn_name: &Ident,
    params: &[FunctionParam],
    label: &str,
) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();

    let async_args: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            quote!(#ident)
        })
        .collect();

    let label_literal = Literal::string(label);

    quote! {
        pub fn #fn_name(#(#param_tokens),*) -> capnp::Result<Vec<u8>> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| capnp::Error::failed(format!(
                    "failed to build runtime for `{}`: {err}",
                    #label_literal
                )))?;
            runtime.block_on(#async_fn_name(#(#async_args),*))
        }
    }
}

fn build_async_function(
    fn_name: &Ident,
    params: &[FunctionParam],
    encoding: Option<&MessageEncodingSpec>,
    label: &str,
) -> TokenStream {
    let param_tokens: Vec<TokenStream> = params
        .iter()
        .map(|param| {
            let ident = &param.ident;
            let ty = &param.ty;
            quote!(#ident: #ty)
        })
        .collect();

    match encoding {
        Some(spec) => {
            let builder_type = &spec.builder_type;
            let assignments = &spec.assignments;
            let root_ident = Ident::new("root", Span::call_site());

            quote! {
                pub async fn #fn_name(#(#param_tokens),*) -> peppylib::PeppyResult<()> {
                    let mut message = capnp::message::Builder::new_default();
                    {
                        let mut #root_ident = message.init_root::<#builder_type>();
                        #(#assignments)*
                    }
                    let mut buffer = Vec::new();
                    capnp::serialize::write_message(&mut buffer, &message)?;

                    let message_payload = to_bytes(message)?;
                    let _node_name = "temporary_node_name";
                    let _ns = "temporary_namespace";
                    let _topic_name = "temporary_topic";
                    let _messenger = peppylib::PeppyMessenger::new(_node_name).await?;

                    // TODO: send_topic_message only applies to topics, `send_service_message` for services and `send_action_message` for actions
                    // messenger.send_topic_message(node_name, ns, topic_name, qos, message_payload)?;
                    Ok(())
                }
            }
        }
        None => {
            let suppress_unused = unused_params_stmt(params);
            let msg = Literal::string(&format!(
                "message format for `{label}` is not available in the generator"
            ));

            quote! {
                pub async fn #fn_name(#(#param_tokens),*) -> capnp::Result<Vec<u8>> {
                    #suppress_unused
                    todo!(#msg);
                }
            }
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
    let format_artifacts = map_message_format(arguments)?;
    let params = collect_function_params(format_artifacts.as_ref(), &struct_prefix, &mut context)?;

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
