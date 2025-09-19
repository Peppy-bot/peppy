mod python;
mod rust;
mod types;

use std::{collections::HashMap, path::Path};

use config::{
    Deployment, DeploymentInstance, DeploymentSource, ExposedService, ExposedTopic, Interfaces,
    Language, MessageFormat, NodeConfig, SubscribedAction, SubscribedService, SubscribedTopic,
    SubscribesTo,
};

use crate::{
    error::{Error, Result},
    generator::types::{AllowedSubscriber, DeploymentMap, NodeSource, SubscriberMap},
};
use python::PythonGenerator;
use rust::RustGenerator;

use types::InterfaceGenerator;

/// Ensures that every Deployment maps to a known node.
/// This is ensured by doing the following:
/// 1. If `Deployment::source` is `DeploymentSource::Local`, look for the node in the provided
///    `nodes` vector. The `name` and the version must match; otherwise return `NodeNotFound`.
/// 2. If `Deployment::source` is `DeploymentSource::Remote`, pull the node from the source (Git or
///    nodes.peppy.bot) or return `NodeNotFound` if the node cannot be pulled. The `name` of the node
///    and `tag` should match; otherwise return `NoMatchingNode`. The pulled nodes are stored inside `<root_dir>/.peppy/nodes`
/// 3. If `Deployment::source` is `DeploymentSource::Network`, expect another root node on the same
///    network to provide it.
pub fn map_deployment_nodes(
    nodes_cache_dir: impl AsRef<Path>,
    deployments: &[Deployment],
    nodes: &[NodeConfig],
) -> Result<Vec<DeploymentMap>> {
    let _ = nodes_cache_dir.as_ref();

    deployments
        .iter()
        .map(|deployment| match &deployment.source {
            DeploymentSource::Local => resolve_local_deployment(deployment, nodes),
            DeploymentSource::Remote(_) => {
                todo!("handle remote deployment sources")
            }
            DeploymentSource::Network => {
                todo!("handle network deployment sources")
            }
        })
        .collect()
}

fn resolve_local_deployment(
    deployment: &Deployment,
    nodes: &[NodeConfig],
) -> Result<DeploymentMap> {
    let node = nodes
        .iter()
        .find(|node| {
            node.manifest.name.as_str() == deployment.name && node.manifest.tag == deployment.tag
        })
        .cloned()
        .ok_or_else(|| Error::NodeNotFound(deployment.name.clone()))?;

    let node_source = NodeSource::new(deployment.source.clone(), node);
    Ok(DeploymentMap::new(deployment.clone(), node_source))
}

/// Once we have a deployment map, we need to ensure nodes `subscribes_to` topic/services/actions of those nodes can map to
pub fn map_deployment_nodes_messages_format(deployment_maps: &[DeploymentMap]) {}

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
    use super::*;

    #[test]
    fn map_deployment_nodes_local_success() {
        let node = sample_node_camera();
        let deployment = sample_deployment();

        let maps = map_deployment_nodes(".", &[deployment.clone()], &[node.clone()])
            .expect("local deployment resolves");

        assert_eq!(maps.len(), 1);
        let map = &maps[0];

        assert_eq!(map.deployment().name, deployment.name);
        assert_eq!(map.deployment().tag, deployment.tag);

        let node_source = map.node_source();
        assert!(matches!(node_source.source(), DeploymentSource::Local));
        assert_eq!(
            node_source.node().manifest.name.as_str(),
            node.manifest.name.as_str()
        );
    }

    #[test]
    fn map_deployment_nodes_local_missing_node() {
        let err = map_deployment_nodes(".", &[sample_deployment()], &[])
            .expect_err("should report missing local node");

        match err {
            Error::NodeNotFound(name) => assert_eq!(name, "uvc_camera"),
            other => panic!("unexpected error: {other:?}"),
        }

        let node = sample_node_lidar();

        let err = map_deployment_nodes(".", &[sample_deployment()], &[node])
            .expect_err("should report missing local node");

        match err {
            Error::NodeNotFound(name) => assert_eq!(name, "uvc_camera"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    fn sample_node_camera() -> NodeConfig {
        serde_json5::from_str(
            r#"{
                manifest: {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    language: "rust"
                }
            }"#,
        )
        .expect("valid node json5")
    }

    fn sample_node_lidar() -> NodeConfig {
        serde_json5::from_str(
            r#"{
                manifest: {
                    name: "lidar",
                    tag: "0.1.0",
                    language: "rust"
                }
            }"#,
        )
        .expect("valid node json5")
    }

    fn sample_deployment() -> Deployment {
        serde_json5::from_str(
            r#"{
                name: "uvc_camera",
                source: "<local>",
                tag: "0.1.0",
                instances: [
                    {
                        namespace: "/"
                    }
                ]
            }"#,
        )
        .expect("valid deployment json5")
    }

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
