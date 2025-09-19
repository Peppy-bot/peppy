mod python;
mod rust;
mod types;

// // TODO: There cannot be more than one emitter with the same name in the same namespace

// /// Called everytime a new change to a peppy configuration is detected
// /// The generated code is stored inside crate::interfaces so during testing do not call this function
// /// directly as this would create throwaway code that will be picked up by git
// pub fn generate_interfaces_code(
//     interfaces: &Interfaces,
//     deployments: &DeploymentInstance,
//     for_language: &Language,
// ) -> Result<Vec<String>> {
//     let generator: Box<dyn InterfaceGenerator> = match for_language {
//         Language::Rust => Box::new(RustGenerator::new()),
//         Language::Python => Box::new(PythonGenerator::new()),
//     };

//     // FIXME: Only `exposes` with a valid deployment and marked as `optional = false` should be considered
//     let exposed_topics = interfaces
//         .exposes
//         .as_ref()
//         .and_then(|exposes| exposes.topics.as_deref())
//         .unwrap_or(&[]);
//     let exposed_services = interfaces
//         .exposes
//         .as_ref()
//         .and_then(|exposes| exposes.services.as_deref())
//         .unwrap_or(&[]);
//     let exposed_actions = interfaces
//         .exposes
//         .as_ref()
//         .and_then(|exposes| exposes.actions.as_deref())
//         .unwrap_or(&[]);

//     let topic_formats = build_format_index(exposed_topics);
//     let service_formats = build_format_index(exposed_services);

//     let subscribed_topics = interfaces
//         .subscribes_to
//         .as_ref()
//         .and_then(|subs| subs.topics.as_deref())
//         .unwrap_or(&[]);
//     let subscribed_services = interfaces
//         .subscribes_to
//         .as_ref()
//         .and_then(|subs| subs.services.as_deref())
//         .unwrap_or(&[]);
//     let subscribed_actions = interfaces
//         .subscribes_to
//         .as_ref()
//         .and_then(|subs| subs.actions.as_deref())
//         .unwrap_or(&[]);

//     let mapped_topics = map_subscribers_to_messages_format(
//         subscribed_topics,
//         &topic_formats,
//         Error::SubscriberTopicMessageFormatMissing,
//     )?;
//     let mapped_services = map_subscribers_to_messages_format(
//         subscribed_services,
//         &service_formats,
//         Error::SubscriberServiceMessageFormatMissing,
//     )?;
//     let mapped_actions = map_subscribers_to_messages_format(
//         subscribed_actions,
//         &topic_formats,
//         Error::SubscriberActionMessageFormatMissing,
//     )?;

//     let mut out = Vec::with_capacity(6);
//     out.push(generator.gen_subscribed_topics(&mapped_topics));
//     out.push(generator.gen_subscribed_services(&mapped_services));
//     out.push(generator.gen_subscribed_actions(&mapped_actions));
//     out.push(generator.gen_exposed_topics(exposed_topics));
//     out.push(generator.gen_exposed_services(exposed_services));
//     out.push(generator.gen_exposed_actions(exposed_actions));

//     Ok(out)
// }

// trait HasName {
//     fn name(&self) -> &str;
// }

// impl HasName for SubscribedTopic {
//     fn name(&self) -> &str {
//         &self.name
//     }
// }

// impl HasName for SubscribedService {
//     fn name(&self) -> &str {
//         &self.name
//     }
// }

// impl HasName for SubscribedAction {
//     fn name(&self) -> &str {
//         &self.name
//     }
// }

// trait ExposedWithFormat {
//     fn name(&self) -> Option<&str>;
//     fn message_format(&self) -> Option<&MessageFormat>;
// }

// impl ExposedWithFormat for ExposedTopic {
//     fn name(&self) -> Option<&str> {
//         self.name.as_deref()
//     }

//     fn message_format(&self) -> Option<&MessageFormat> {
//         self.message_format.as_ref()
//     }
// }

// impl ExposedWithFormat for ExposedService {
//     fn name(&self) -> Option<&str> {
//         self.name.as_deref()
//     }

//     fn message_format(&self) -> Option<&MessageFormat> {
//         self.message_format.as_ref()
//     }
// }

// fn build_format_index(exposed: &[impl ExposedWithFormat]) -> HashMap<String, MessageFormat> {
//     exposed
//         .iter()
//         .filter_map(|item| {
//             item.name()
//                 .zip(item.message_format().cloned())
//                 .map(|(name, format)| (name.to_owned(), format))
//         })
//         .collect()
// }

// fn map_subscribers_to_messages_format<T, F>(
//     subscribers: &[T],
//     formats: &HashMap<String, MessageFormat>,
//     missing_err: F,
// ) -> Result<Vec<SubscriberMap<T>>>
// where
//     T: AllowedSubscriber + Clone + HasName,
//     F: Fn(String) -> Error,
// {
//     // TODO: Double check, this is generated code and might be wrong
//     subscribers
//         .iter()
//         .cloned()
//         .map(|subscriber| {
//             let format = formats
//                 .get(subscriber.name())
//                 .cloned()
//                 .ok_or_else(|| missing_err(subscriber.name().to_owned()))?;
//             Ok(SubscriberMap::new(subscriber, format))
//         })
//         .collect()
// }

#[cfg(test)]
mod tests {

    // #[test]
    // fn generate_interfaces_code_rust_success() {
    //     let interfaces: Interfaces = serde_json5::from_str(
    //         r#"{
    //             exposes: {
    //                 topics: [
    //                     {
    //                         name: "stream",
    //                         qos_profile: "sensor_data",
    //                         message_format: {
    //                             header: {
    //                                 stamp: "time",
    //                                 frame_id: "u32",
    //                             },
    //                             encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
    //                             width: "u32",
    //                             height: "u32",
    //                             image: [
    //                                 "u8",
    //                                 "u8",
    //                                 "u8"
    //                             ],
    //                         },
    //                     }
    //                 ],
    //             },
    //             subscribes_to: {
    //                 topics: [
    //                     {
    //                         node: "uvc_camera",
    //                         tag: "0.1.0",
    //                         name: "stream",
    //                         namespace: "/",
    //                         callback: "on_handle_video_frame",
    //                         optional: false,
    //                     }
    //                 ],
    //             },
    //         }"#,
    //     )
    //     .expect("valid JSON5 interfaces structure");

    //     // let code = generate_interfaces_code(&interfaces, &Language::Rust)
    //     //     .expect("interfaces generation to succeed");

    //     // assert_eq!(code.len(), 6);
    //     // assert!(code[0].contains("impl Topics"));
    //     // assert!(code[3].contains("exposed_topic_0"));
    //     todo!("Finish")
    // }

    // #[test]
    // fn generate_interfaces_code_rust_missing_topic_format() {
    //     let interfaces: Interfaces = serde_json5::from_str(
    //         r#"{
    //             exposes: {
    //                 topics: [
    //                     {
    //                         name: "stream",
    //                         qos_profile: "sensor_data",
    //                         message_format: {
    //                             header: {
    //                                 stamp: "time",
    //                                 frame_id: "u32",
    //                             },
    //                             encoding: "string",
    //                             width: "u32",
    //                             height: "u32",
    //                             image: ["u8", "u8", "u8"],
    //                         },
    //                     }
    //                 ],
    //             },
    //             subscribes_to: {
    //                 topics: [
    //                     {
    //                         node: "uvc_camera",
    //                         tag: "0.1.0",
    //                         name: "orphan_topic",
    //                         namespace: "/",
    //                         callback: "on_handle_video_frame",
    //                         optional: false,
    //                     }
    //                 ],
    //             },
    //         }"#,
    //     )
    //     .expect("valid JSON5 interfaces structure");

    //     // let err = generate_interfaces_code(&interfaces, &Language::Rust)
    //     //     .expect_err("missing topic mapping should error");

    //     // match err {
    //     //     Error::SubscriberTopicMessageFormatMissing(name) => {
    //     //         assert_eq!(name, "orphan_topic")
    //     //     }
    //     //     other => panic!("unexpected error: {other:?}"),
    //     // }
    //     todo!("Finish")
    // }
}
