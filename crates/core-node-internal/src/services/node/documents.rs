//! The `instance_documents` service: the pinned documents of one instance,
//! the manifest the stack holds and the contract and pairing documents its
//! code was generated from, as the entity of its node keeps them, with the
//! document of each daemon interface its daemon targets name, as compiled
//! into peppy.

use crate::Result;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use config::node::NodeConfig;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{self, InstanceDocuments, InstanceDocumentsRequest};
use core_node_api::names;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::repository::{PinKind, PinnedDocument};
use node_stack::NodeStack;
use peppylib::messaging::{Failure, SenderTarget, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_instance_documents(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::InstanceDocuments.name(),
    )
    .await?;
    let core_node = core_node_name.to_owned();
    Ok(tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                std::future::ready(answer(&core_node, &node_stack, &context))
            })
            .await
            .map_err(Into::into)
    }))
}

/// The documents of the instance the request names: a request that does not
/// decode, or names no instance of the stack, is `rejected`.
fn answer(
    core_node: &str,
    node_stack: &NodeStack,
    context: &ServiceRequestContext,
) -> ServiceAnswer {
    let request = InstanceDocumentsRequest::decode(context.message().payload_bytes().as_ref())
        .map_err(|e| failure(PeppyFailureKind::Rejected, e))?;
    debug!(
        "Received `instance_documents` request from {} for {}",
        context.message().instance_id(),
        request.instance_id
    );
    let not_found = || {
        failure(
            PeppyFailureKind::Rejected,
            format!(
                "Node instance '{}' not found in node stack",
                request.instance_id
            ),
        )
    };
    let instance_id = Name::new(&request.instance_id).map_err(|_| not_found())?;
    let (_, entity) = node_stack
        .find_instance_and_entity_any_state(&instance_id)
        .ok_or_else(not_found)?;
    let documents = {
        let guard = entity.read();
        instance_documents(
            core_node,
            &request.instance_id,
            guard.config(),
            guard.documents(),
        )?
    };
    documents.encode().map_err(|e| {
        failure(
            PeppyFailureKind::AttemptFailed,
            format!("failed to encode the documents of the instance: {e}"),
        )
    })
}

/// The documents of an instance of the node with `config`, whose entity
/// keeps `documents`: the manifest as the stack holds it, then the contract
/// documents in the order of `documents` followed by the document of each
/// daemon target in the order of `depends_on.daemon`, and the pairing
/// documents in the order of `documents`. The document of a daemon target
/// ships with peppy, so the entity keeps no pin of it.
fn instance_documents(
    core_node: &str,
    instance_id: &str,
    config: &NodeConfig,
    documents: &[PinnedDocument],
) -> std::result::Result<InstanceDocuments, Failure> {
    let manifest = json5_pretty::to_string_pretty(config).map_err(|e| {
        failure(
            PeppyFailureKind::AttemptFailed,
            format!("cannot serialize the manifest of the instance: {e}"),
        )
    })?;
    let mut contracts = Vec::new();
    let mut pairings = Vec::new();
    for document in documents {
        let sent = encoding::PinnedDocument::new(
            document.pin.name.as_str(),
            document.pin.tag.as_str(),
            document.content.clone(),
        );
        match document.pin.kind {
            PinKind::Contract => contracts.push(sent),
            PinKind::Pairing => pairings.push(sent),
            PinKind::Node | PinKind::McpExposure => {
                return Err(failure(
                    PeppyFailureKind::AttemptFailed,
                    format!(
                        "{} is not a document a manifest names",
                        document.pin.label()
                    ),
                ));
            }
        }
    }
    for target in config
        .manifest
        .depends_on
        .iter()
        .flat_map(|deps| &deps.daemon)
    {
        let interface = DaemonInterface::of_target(target).map_err(|error| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("daemon target `{}`: {error}", target.link_id),
            )
        })?;
        contracts.push(encoding::PinnedDocument::new(
            interface.name(),
            interface.tag(),
            interface.document_text().to_owned(),
        ));
    }
    Ok(InstanceDocuments {
        core_node: core_node.to_owned(),
        instance_id: instance_id.to_owned(),
        manifest: encoding::PinnedDocument::new(
            config.manifest.name.as_str(),
            config.manifest.tag.as_str(),
            manifest,
        ),
        contracts,
        pairings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::EntryOrigin;
    use config::node::NodeConfigParser;
    use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint, PinnedItem};
    use std::path::PathBuf;

    const ARM: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "arm", tag: "v1", description: ["A test node."] },
        interfaces: {},
        execution: { language: "rust", run_cmd: ["./bin"] },
    }"#;

    fn document(kind: PinKind, name: &str) -> PinnedDocument {
        let content = format!(r#"{{ manifest: {{ name: "{name}", tag: "v1" }} }}"#);
        PinnedDocument::new(
            PinnedItem {
                kind,
                name: ItemName::parse(name).expect("valid name"),
                tag: ItemTag::parse("v1").expect("valid tag"),
                sha256: ManifestFingerprint::of_bytes(content.as_bytes()),
                origin: EntryOrigin::Fs {
                    path: PathBuf::from(format!("/no/such/hub/{name}.json5")),
                },
            },
            content,
        )
    }

    fn sent(document: &PinnedDocument) -> encoding::PinnedDocument {
        encoding::PinnedDocument::new(
            document.pin.name.as_str(),
            document.pin.tag.as_str(),
            document.content.clone(),
        )
    }

    /// An instance gives the documents its entity keeps, contracts and
    /// pairings apart, each in the order of the entity, with no read of
    /// their origins: the paths of these documents do not exist.
    #[test]
    fn the_documents_of_an_instance_are_those_its_entity_keeps() {
        let config = NodeConfigParser::from_content(ARM).expect("the manifest parses");
        let limb_motion = document(PinKind::Contract, "limb_motion");
        let arm_link = document(PinKind::Pairing, "arm_link");
        let camera = document(PinKind::Contract, "camera");

        let documents = instance_documents(
            "robot",
            "arm_inst",
            &config,
            &[limb_motion.clone(), arm_link.clone(), camera.clone()],
        )
        .expect("the documents are in hand");

        assert_eq!(
            (documents.core_node.as_str(), documents.instance_id.as_str()),
            ("robot", "arm_inst")
        );
        assert_eq!(
            (documents.manifest.name(), documents.manifest.tag()),
            ("arm", "v1")
        );
        assert_eq!(documents.contracts, [sent(&limb_motion), sent(&camera)]);
        assert_eq!(documents.pairings, [sent(&arm_link)]);
    }

    /// The document of each daemon target comes after the pinned contract
    /// documents, as compiled into peppy, under the name and tag of its
    /// interface.
    #[test]
    fn the_documents_of_an_instance_give_the_document_of_each_daemon_target() {
        let config = NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "director", tag: "v1", description: ["A test node."], depends_on: {
                    contracts: [{ name: "limb_motion", tag: "v1", link_id: "arm" }],
                    daemon: [{ name: "robot_leases", tag: "v1", link_id: "fleet" }],
                } },
                interfaces: { actions: { consumes: [{ link_id: "fleet", name: "lease" }] } },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        )
        .expect("the manifest parses");
        let limb_motion = document(PinKind::Contract, "limb_motion");

        let documents = instance_documents(
            "robot",
            "director_inst",
            &config,
            std::slice::from_ref(&limb_motion),
        )
        .expect("the documents are in hand");

        assert_eq!(
            documents.contracts,
            [
                sent(&limb_motion),
                encoding::PinnedDocument::new(
                    "robot_leases",
                    "v1",
                    DaemonInterface::RobotLeases.document_text().to_owned(),
                ),
            ]
        );
        assert!(documents.pairings.is_empty());
    }
}
