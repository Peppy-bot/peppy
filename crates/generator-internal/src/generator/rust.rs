use super::types::{InterfaceArtifact, InterfaceBackend, InterfaceKind};
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SchemaType, SubscribedAction,
    SubscribedService, SubscribedTopic, TypeToken,
};
use proc_macro2::{Ident, Literal, Span, TokenStream};
use quote::quote;
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
        let fn_name = prefixed_ident("", non_empty_str(topic.name.as_str()), "topic");
        let fn_name_str = fn_name.to_string();
        let async_fn_name = Ident::new(&(fn_name_str.clone() + "_async"), Span::call_site());

        let struct_prefix = to_camel_case(&fn_name_str);

        let mut context = GenerationContext::default();
        let params =
            collect_function_params(topic.message_format.as_ref(), &struct_prefix, &mut context);
        let struct_tokens = context.into_tokens();

        let param_tokens: Vec<TokenStream> = params
            .iter()
            .map(|param| {
                let ident = &param.ident;
                let ty = &param.ty;
                quote!(#ident: #ty)
            })
            .collect();

        let suppress_unused = unused_params_stmt(&params);
        let suppress_unused_async = suppress_unused.clone();

        let tokens: TokenStream = quote! {
            #( #struct_tokens )*

            impl Topics {
                pub fn #fn_name(#(#param_tokens),*) {
                    #suppress_unused
                    todo!("publish PMI topic synchronously");
                }

                pub async fn #async_fn_name(#(#param_tokens),*) {
                    #suppress_unused_async
                    todo!("publish PMI topic asynchronously");
                }
            }
        };
        let rendered = parse2::<File>(tokens.clone())
            .map(|file| prettyplease::unparse(&file))
            .unwrap_or_else(|_| tokens.to_string());

        self.push_section(InterfaceArtifact::from_kind(
            InterfaceKind::ExposedTopic,
            rendered,
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
    use config::node::CallbackName;
    use config::node::ExposedTopic;

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

    // `RustGenerator::add_exposed_topic` should generate Rust code, a sync and async version, it uses `ExposedTopic::message_format` to determine the input parameters of the sync and async functions that need to be generated.
    // For example, the user of the Rust generated code should be able to call:
    // ```
    // peppygen::topics::uvc_camera::push_frame(header, encoding, width, height, image);
    // ```
    // when the exposed topic look like this:
    // ```
    //         {
    //             name: "push_frame", // The name of the topic inside the `uvc_camera` node
    //             qos_profile: "sensor_data",
    //             message_format: {
    //                 header: {
    //                     stamp: "time",
    //                     frame_id: "u32",
    //                 },
    //                 encoding: "string",
    //                 width: "u32",
    //                 height: "u32",
    //                 image: {
    //                     type: "array",
    //                     items: "u8",
    //                     length: 3,
    //                 },
    //             },
    //         }
    // ```
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
