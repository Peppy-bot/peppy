//! The joinable nodes of a launch: every node that a join of an option of a
//! `stack_copies` scope can bring. The launch checks of the scope compose one
//! trial copy of each feature set of each scoped option over the bare stack,
//! and the nodes of these copies are the joinable nodes. The nodes of a copy
//! do not depend on the other copies of the stack, so the set is exact and
//! closed when the launch starts.

use super::types::{Deployment, DeploymentSource};
use config::runtime::Name;

/// One node that a join of a scoped option can bring, and the scoped
/// options whose copies bring it.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinableNode {
    /// The node's deployment, as the first trial copy that brings the node
    /// composes it: its source names the node, and its instances are those of
    /// that trial copy.
    pub deployment: Deployment,
    /// The scoped options whose copies bring the node, in the order the
    /// scopes name them.
    pub options: Vec<Name>,
}

/// The joinable nodes of a launch, one per source, in the order the trial
/// copies first bring them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JoinableNodes(Vec<JoinableNode>);

impl JoinableNodes {
    /// Adds the nodes of a trial copy of `option`. A node already held gains
    /// the option. A deployment of the built-in MCP server builds nothing, so
    /// it is no joinable node.
    pub fn add_copy(&mut self, option: &Name, deployments: Vec<Deployment>) {
        for deployment in deployments {
            if !matches!(deployment.source, DeploymentSource::Node { .. }) {
                continue;
            }
            match self
                .0
                .iter_mut()
                .find(|node| node.deployment.source == deployment.source)
            {
                Some(node) if node.options.contains(option) => {}
                Some(node) => node.options.push(option.clone()),
                None => self.0.push(JoinableNode {
                    deployment,
                    options: vec![option.clone()],
                }),
            }
        }
    }

    /// Adds the nodes of `other`, as [`Self::add_copy`] adds those of a copy.
    pub fn merge(&mut self, other: Self) {
        for node in other.0 {
            for option in &node.options {
                self.add_copy(option, vec![node.deployment.clone()]);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &JoinableNode> {
        self.0.iter()
    }
}

impl IntoIterator for JoinableNodes {
    type Item = JoinableNode;
    type IntoIter = std::vec::IntoIter<JoinableNode>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::launcher::DeploymentInstance;

    fn name(value: &str) -> Name {
        Name::new(value).expect("a name")
    }

    fn node(source: &str, instance: &str) -> Deployment {
        Deployment {
            source: DeploymentSource::Node {
                name: source.to_owned(),
                tag: "v1".to_owned(),
            },
            instances: vec![DeploymentInstance::empty(name(instance))],
        }
    }

    fn summary(nodes: &JoinableNodes) -> Vec<(String, Vec<String>)> {
        nodes
            .iter()
            .map(|node| {
                (
                    node.deployment.source.label(),
                    node.options.iter().map(ToString::to_string).collect(),
                )
            })
            .collect()
    }

    /// A node that several copies bring is held once, with each option that
    /// brings it once, and keeps the deployment of the first copy.
    #[test]
    fn a_node_is_held_once_with_every_option_that_brings_it() {
        let mut nodes = JoinableNodes::default();
        nodes.add_copy(
            &name("openarm_sim"),
            vec![node("openarm", "check_arm"), node("camera", "check_cam")],
        );
        nodes.add_copy(
            &name("openarm_sim"),
            vec![node("openarm", "check_arm"), node("brain", "check_brain")],
        );
        nodes.add_copy(&name("so101_sim"), vec![node("camera", "other_cam")]);

        assert_eq!(
            summary(&nodes),
            [
                ("openarm:v1".to_owned(), vec!["openarm_sim".to_owned()]),
                (
                    "camera:v1".to_owned(),
                    vec!["openarm_sim".to_owned(), "so101_sim".to_owned()]
                ),
                ("brain:v1".to_owned(), vec!["openarm_sim".to_owned()]),
            ]
        );
        let camera = nodes.iter().nth(1).expect("the camera");
        assert_eq!(
            camera.deployment.instances[0].instance_id.as_str(),
            "check_cam"
        );
    }

    /// The built-in MCP server of a copy builds nothing.
    #[test]
    fn an_exposure_deployment_is_no_joinable_node() {
        let exposures = Deployment {
            source: DeploymentSource::Exposures {
                exposures: vec![
                    crate::internal::source::ExposureRef::parse("camera_endpoint:v1").unwrap(),
                ],
            },
            instances: vec![DeploymentInstance::empty(name("check_mcp"))],
        };
        let mut nodes = JoinableNodes::default();
        nodes.add_copy(&name("openarm_sim"), vec![exposures]);
        assert!(nodes.is_empty());
    }

    /// The nodes of two scopes are their union, in the order the first scope
    /// then the second bring them.
    #[test]
    fn merged_scopes_are_the_union_of_their_nodes() {
        let mut first = JoinableNodes::default();
        first.add_copy(&name("openarm_sim"), vec![node("openarm", "a")]);
        let mut second = JoinableNodes::default();
        second.add_copy(&name("so101_sim"), vec![node("so101", "b")]);
        second.add_copy(&name("openarm_sim"), vec![node("openarm", "c")]);

        first.merge(second);
        assert_eq!(
            summary(&first),
            [
                ("openarm:v1".to_owned(), vec!["openarm_sim".to_owned()]),
                ("so101:v1".to_owned(), vec!["so101_sim".to_owned()]),
            ]
        );
    }
}
