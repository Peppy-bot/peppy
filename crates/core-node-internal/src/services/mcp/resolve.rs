//! From exposure references or pins to a planned built-in deployment.

use crate::services::repo::cache::{self as repo_cache, ContractCacheEntry, McpExposureCacheEntry};
use daemon_config::consts::PeppyDirs;
use daemon_config::mcp_deployment::{
    McpDeploymentPlan, McpServeSpec, PinnedDocument, plan_deployment,
};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::{
    EntryOrigin, ItemName, ItemTag, ManifestFingerprint, PinKind, PinnedItem,
};
use daemon_config::source::ExposureRef;
use std::collections::BTreeMap;

/// A built-in deployment with every document it needs in hand: the spec the
/// server reads, and the plan the launch binds and pins.
#[derive(Debug)]
pub(crate) struct ResolvedMcpDeployment {
    pub spec: McpServeSpec,
    pub plan: McpDeploymentPlan,
}

impl ResolvedMcpDeployment {
    pub fn exposure_pins(&self) -> Vec<PinnedItem> {
        self.spec.exposures.iter().map(|d| d.pin.clone()).collect()
    }

    pub fn contract_pins(&self) -> Vec<PinnedItem> {
        self.spec.contracts.iter().map(|d| d.pin.clone()).collect()
    }

    fn from_spec(spec: McpServeSpec) -> Result<Self, String> {
        let (exposures, contracts) = spec.resolve()?;
        let plan = plan_deployment(&exposures, &contracts).map_err(|e| e.to_string())?;
        Ok(Self { spec, plan })
    }
}

fn pinned(
    kind: PinKind,
    name: &ItemName,
    tag: &ItemTag,
    sha256: &ManifestFingerprint,
    origin: &EntryOrigin,
) -> PinnedItem {
    PinnedItem {
        kind,
        name: name.clone(),
        tag: tag.clone(),
        sha256: sha256.clone(),
        origin: origin.clone(),
    }
}

fn utf8(label: &str, bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|e| format!("{label} is not UTF-8: {e}"))
}

/// Resolves the exposures a launcher lists through this machine's own cache
/// rules, then every contract they reference, and plans the deployment.
///
/// A contract is looked up once per identity. An exposure's own `sha256`
/// pin selects the content for that identity; two exposures pinning one
/// contract at different bytes are refused naming both, before anything is
/// read, since no closure can carry both.
pub(crate) fn resolve_exposure_deployment(
    peppy_dirs: &PeppyDirs,
    references: &[ExposureRef],
    on_feedback: &dyn Fn(&str),
) -> Result<ResolvedMcpDeployment, String> {
    let exposure_entries = repo_cache::load_repo_cache::<McpExposureCacheEntry>(peppy_dirs)
        .map_err(|e| format!("failed to load the exposure cache: {e}"))?;
    let contract_entries = repo_cache::load_contract_cache(peppy_dirs)
        .map_err(|e| format!("failed to load the contract cache: {e}"))?;

    let mut exposures = Vec::with_capacity(references.len());
    // Contract identity -> (author pin, the exposure that stated it).
    let mut wanted: BTreeMap<(String, String), (Option<ManifestFingerprint>, String)> =
        BTreeMap::new();
    for reference in references {
        let (entry, bytes) = repo_cache::resolve_cached_doc_entry(
            peppy_dirs,
            &exposure_entries,
            &reference.name,
            &reference.tag,
            None,
            on_feedback,
        )?;
        let content = utf8(&format!("exposure `{reference}`"), bytes)?;
        let document = PeppyMcpExposureParser::from_content(&content)
            .map_err(|e| format!("exposure `{reference}` does not parse: {e}"))?;
        for target in document.targets.values() {
            let key = (
                target.contract.name.as_str().to_owned(),
                target.contract.tag.clone(),
            );
            match wanted.get_mut(&key) {
                None => {
                    wanted.insert(key, (target.contract.sha256.clone(), reference.to_string()));
                }
                Some((author, by)) => match (&*author, &target.contract.sha256) {
                    (Some(first), Some(second)) if first != second => {
                        return Err(format!(
                            "exposures `{by}` and `{reference}` pin contract `{}:{}` at different \
                             bytes (`{first}` and `{second}`); one deployment carries one content \
                             per contract",
                            key.0, key.1
                        ));
                    }
                    (None, Some(second)) => {
                        *author = Some(second.clone());
                        *by = reference.to_string();
                    }
                    _ => {}
                },
            }
        }
        exposures.push(PinnedDocument::new(
            pinned(
                PinKind::McpExposure,
                &entry.exposure_name,
                &entry.tag,
                &entry.sha256,
                &entry.origin,
            ),
            content,
        ));
    }

    let mut contracts = Vec::with_capacity(wanted.len());
    for ((name, tag), (author, _)) in &wanted {
        let (entry, bytes) = repo_cache::resolve_cached_doc_entry(
            peppy_dirs,
            &contract_entries,
            name,
            tag,
            author.as_ref().map(ManifestFingerprint::as_str),
            on_feedback,
        )?;
        contracts.push(PinnedDocument::new(
            pinned(
                PinKind::Contract,
                &entry.contract_name,
                &entry.tag,
                &entry.sha256,
                &entry.origin,
            ),
            utf8(&format!("contract `{name}:{tag}`"), bytes)?,
        ));
    }

    ResolvedMcpDeployment::from_spec(McpServeSpec {
        exposures,
        contracts,
    })
}

/// The plan alone, for callers that check a launcher without launching it.
pub fn resolve_exposure_plan(
    peppy_dirs: &PeppyDirs,
    references: &[ExposureRef],
    on_feedback: &dyn Fn(&str),
) -> Result<McpDeploymentPlan, String> {
    resolve_exposure_deployment(peppy_dirs, references, on_feedback).map(|resolved| resolved.plan)
}

/// Materializes a coordinator's decision: the pinned exposure and contract
/// bytes, from this machine's copies on a content match and from each pin's
/// origin otherwise, then plans the deployment. Nothing is resolved by name.
pub(crate) fn materialize_exposure_deployment(
    peppy_dirs: &PeppyDirs,
    exposure_pins: &[PinnedItem],
    contract_pins: &[PinnedItem],
    on_feedback: &dyn Fn(&str),
) -> Result<ResolvedMcpDeployment, String> {
    let exposure_entries = repo_cache::load_repo_cache::<McpExposureCacheEntry>(peppy_dirs)
        .map_err(|e| format!("failed to load the exposure cache: {e}"))?;
    let contract_entries = repo_cache::load_repo_cache::<ContractCacheEntry>(peppy_dirs)
        .map_err(|e| format!("failed to load the contract cache: {e}"))?;
    let mut exposures = Vec::with_capacity(exposure_pins.len());
    for pin in exposure_pins {
        let (_, bytes) =
            repo_cache::resolve_pin_to_bytes(peppy_dirs, &exposure_entries, pin, on_feedback)?;
        exposures.push(PinnedDocument::new(pin.clone(), utf8(&pin.label(), bytes)?));
    }
    let mut contracts = Vec::with_capacity(contract_pins.len());
    for pin in contract_pins {
        let (_, bytes) =
            repo_cache::resolve_pin_to_bytes(peppy_dirs, &contract_entries, pin, on_feedback)?;
        contracts.push(PinnedDocument::new(pin.clone(), utf8(&pin.label(), bytes)?));
    }
    ResolvedMcpDeployment::from_spec(McpServeSpec {
        exposures,
        contracts,
    })
}
