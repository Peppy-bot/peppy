//! The `instance_documents` service: the pinned documents of one instance,
//! the manifest the stack holds and the contract and pairing documents its
//! code was generated from, read from the document caches by their pins.

use super::Report;
use super::pins::load_doc_cache_if_needed;
use crate::Result;
use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, PairingCacheEntry, RepoCacheEntry,
};
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{InstanceDocuments, InstanceDocumentsRequest, PinnedDocument};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::{PinKind, PinnedItem};
use node_stack::NodeStack;
use peppylib::messaging::{Failure, SenderTarget, ServiceAnswer, ServiceRequestContext};
use peppylib::{MessengerHandle, ServiceMessenger};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tracing::debug;

/// How long one request may take to read the documents of an instance,
/// fetches from their origins included. It is shorter than the 120 s that
/// `peppy node call`, `echo` and `describe` wait, so a read that takes too
/// long reaches the command as a `timeout` reply that says so. The service
/// answers one request at a time: a request that waits behind a slow read
/// can still reach the end of the command's wait first.
const DOCUMENTS_READ_TIMEOUT: Duration = Duration::from_secs(100);

pub async fn listen_for_instance_documents(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
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
                let reading = DocumentReading {
                    core_node: core_node.clone(),
                    node_stack: Arc::clone(&node_stack),
                    peppy_dirs: peppy_dirs.clone(),
                };
                async move {
                    tokio::time::timeout(DOCUMENTS_READ_TIMEOUT, reading.answer(&context))
                        .await
                        .unwrap_or_else(|_| {
                            Err(failure(
                                PeppyFailureKind::Timeout,
                                format!(
                                    "reading the documents of the instance took longer than {}s",
                                    DOCUMENTS_READ_TIMEOUT.as_secs()
                                ),
                            ))
                        })
                }
            })
            .await
            .map_err(Into::into)
    }))
}

/// What one request reads its documents from.
struct DocumentReading {
    core_node: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
}

impl DocumentReading {
    /// The documents of the instance the request names: a request that does
    /// not decode, or names no instance of the stack, is `rejected`; a
    /// document the caches can no longer give is `attempt_failed`.
    async fn answer(self, context: &ServiceRequestContext) -> ServiceAnswer {
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
        let (_, entity) = self
            .node_stack
            .find_instance_and_entity_any_state(&instance_id)
            .ok_or_else(not_found)?;
        let (config, pins) = {
            let guard = entity.read();
            (guard.config().clone(), guard.document_pins().to_vec())
        };

        let manifest = json5_pretty::to_string_pretty(&config).map_err(|e| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("cannot serialize the manifest of the instance: {e}"),
            )
        })?;
        let peppy_dirs = self.peppy_dirs.clone();
        let (contracts, pairings) =
            tokio::task::spawn_blocking(move || read_pinned_documents(&peppy_dirs, &pins))
                .await
                .map_err(|e| {
                    failure(
                        PeppyFailureKind::AttemptFailed,
                        format!("the document read failed: {e}"),
                    )
                })??;

        InstanceDocuments {
            core_node: self.core_node,
            instance_id: request.instance_id,
            manifest: PinnedDocument::new(
                config.manifest.name.as_str(),
                config.manifest.tag.as_str(),
                manifest,
            ),
            contracts,
            pairings,
        }
        .encode()
        .map_err(|e| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("failed to encode the documents of the instance: {e}"),
            )
        })
    }
}

/// The contract documents and the pairing documents of `pins`, in pin order,
/// each read from its cache by its pin. Blocking: a pin whose local copy is
/// gone or drifted is fetched from its origin. A cache is loaded only when a
/// pin of its kind needs it, so a cache that does not parse fails only the
/// instances that read from it.
fn read_pinned_documents(
    peppy_dirs: &PeppyDirs,
    pins: &[PinnedItem],
) -> std::result::Result<(Vec<PinnedDocument>, Vec<PinnedDocument>), Failure> {
    let unreadable = |reason: String| failure(PeppyFailureKind::AttemptFailed, reason);
    let needs = |kind: PinKind| pins.iter().any(|pin| pin.kind == kind);
    let contract_cache: Vec<ContractCacheEntry> =
        load_doc_cache_if_needed(peppy_dirs, needs(PinKind::Contract)).map_err(unreadable)?;
    let pairing_cache: Vec<PairingCacheEntry> =
        load_doc_cache_if_needed(peppy_dirs, needs(PinKind::Pairing)).map_err(unreadable)?;
    let mut contracts = Vec::new();
    let mut pairings = Vec::new();
    for pin in pins {
        match pin.kind {
            PinKind::Contract => {
                contracts.push(read_pinned_document(peppy_dirs, &contract_cache, pin)?);
            }
            PinKind::Pairing => {
                pairings.push(read_pinned_document(peppy_dirs, &pairing_cache, pin)?);
            }
            PinKind::Node | PinKind::McpExposure => {
                return Err(unreadable(format!(
                    "{} is not a document a manifest names",
                    pin.label()
                )));
            }
        }
    }
    Ok((contracts, pairings))
}

fn read_pinned_document<E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    cache: &[E],
    pin: &PinnedItem,
) -> std::result::Result<PinnedDocument, Failure> {
    let unreadable = |reason: String| failure(PeppyFailureKind::AttemptFailed, reason);
    let on_feedback = |report: Report<'_>| debug!("{}", report.text());
    let (path, bytes) = repo_cache::resolve_pin_to_bytes(peppy_dirs, cache, pin, &on_feedback)
        .map_err(|reason| {
            unreadable(format!(
                "the daemon can no longer read {}: {reason}",
                pin.label()
            ))
        })?;
    let content = String::from_utf8(bytes).map_err(|e| {
        unreadable(format!(
            "{}: content at {} is not UTF-8: {e}",
            pin.label(),
            path.display()
        ))
    })?;
    Ok(PinnedDocument::new(
        pin.name.as_str(),
        pin.tag.as_str(),
        content,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::EntryOrigin;
    use daemon_config::repository::{ItemName, ItemTag, ManifestFingerprint};

    /// A document cache is loaded only when a pin needs it: a contract cache
    /// that does not parse fails no read of an instance whose documents are
    /// pairings, and fails the read of one that needs a contract.
    #[test]
    fn a_cache_that_does_not_parse_fails_only_the_reads_that_need_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let peppy_dirs = PeppyDirs::new(dir.path());
        let contract_cache = repo_cache::contracts_repo_cache_path(&peppy_dirs);
        std::fs::create_dir_all(contract_cache.parent().expect("a cache dir"))
            .expect("create the cache dir");
        std::fs::write(&contract_cache, "not a cache {").expect("write the contract cache");
        let document_path = dir.path().join("arm_link.json5");
        let document = r#"{ manifest: { name: "arm_link", tag: "v1" } }"#;
        std::fs::write(&document_path, document).expect("write the document");
        let pin = |kind: PinKind| PinnedItem {
            kind,
            name: ItemName::parse("arm_link").expect("valid name"),
            tag: ItemTag::parse("v1").expect("valid tag"),
            sha256: ManifestFingerprint::of_bytes(document.as_bytes()),
            origin: EntryOrigin::Fs {
                path: document_path.clone(),
            },
        };

        let (contracts, pairings) = read_pinned_documents(&peppy_dirs, &[pin(PinKind::Pairing)])
            .expect("the pairing reads with no contract cache");
        assert!(contracts.is_empty());
        assert_eq!(pairings, [PinnedDocument::new("arm_link", "v1", document)]);

        let error = read_pinned_documents(&peppy_dirs, &[pin(PinKind::Contract)])
            .expect_err("a contract pin needs the contract cache");
        assert!(
            error.message().contains("failed to load contract cache"),
            "{}",
            error.message()
        );
    }
}
