//! From exposure references or pins to a planned built-in deployment.

use crate::services::repo::cache::{
    self as repo_cache, ContractCacheEntry, McpExposureCacheEntry, PinnableCacheEntry,
    RepoCacheEntry,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::mcp_deployment::{
    McpDeploymentPlan, McpServeSpec, PinnedDocument, plan_deployment,
};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::{DeploymentPins, DeploymentRoot, ManifestFingerprint, PinnedItem};
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

/// Resolves one `name:tag[@sha256]` reference through this machine's cache
/// rules into the document's bytes beside the pin its winning entry mints:
/// what a coordinator ships, where [`repo_cache::resolve_cached_doc`] would
/// hand back a parsed value.
pub(super) fn resolve_cached_document<E: PinnableCacheEntry>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> Result<PinnedDocument, String> {
    let (entry, bytes) = repo_cache::resolve_cached_doc_entry(
        peppy_dirs,
        entries,
        name,
        tag,
        sha256_pin,
        on_feedback,
    )?;
    let content = String::from_utf8(bytes)
        .map_err(|e| format!("cached {} `{name}:{tag}` is not UTF-8: {e}", E::KIND))?;
    Ok(PinnedDocument::new(entry.pin(), content))
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
        let exposure = resolve_cached_document(
            peppy_dirs,
            &exposure_entries,
            &reference.name,
            &reference.tag,
            None,
            on_feedback,
        )?;
        let document = PeppyMcpExposureParser::from_content(&exposure.content)
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
        exposures.push(exposure);
    }

    let mut contracts = Vec::with_capacity(wanted.len());
    for ((name, tag), (author, _)) in &wanted {
        contracts.push(resolve_cached_document(
            peppy_dirs,
            &contract_entries,
            name,
            tag,
            author.as_ref().map(ManifestFingerprint::as_str),
            on_feedback,
        )?);
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
    pins: &DeploymentPins,
    on_feedback: &dyn Fn(&str),
) -> Result<ResolvedMcpDeployment, String> {
    let DeploymentRoot::Exposures(exposure_pins) = &pins.root else {
        return Err(format!(
            "{} is a node deployment, not a built-in MCP one",
            pins.root.label()
        ));
    };
    let exposure_entries = repo_cache::load_repo_cache::<McpExposureCacheEntry>(peppy_dirs)
        .map_err(|e| format!("failed to load the exposure cache: {e}"))?;
    let contract_entries = repo_cache::load_repo_cache::<ContractCacheEntry>(peppy_dirs)
        .map_err(|e| format!("failed to load the contract cache: {e}"))?;
    ResolvedMcpDeployment::from_spec(McpServeSpec {
        exposures: materialize_pinned(peppy_dirs, &exposure_entries, exposure_pins, on_feedback)?,
        contracts: materialize_pinned(peppy_dirs, &contract_entries, &pins.closure, on_feedback)?,
    })
}

/// Every pin's bytes, through the same content-first rule a pinned node's
/// documents are materialized by.
fn materialize_pinned<E: RepoCacheEntry>(
    peppy_dirs: &PeppyDirs,
    entries: &[E],
    pins: &[PinnedItem],
    on_feedback: &dyn Fn(&str),
) -> Result<Vec<PinnedDocument>, String> {
    pins.iter()
        .map(|pin| {
            let content = repo_cache::resolve_pinned_doc(
                peppy_dirs,
                entries,
                pin,
                |content| Ok(content.to_owned()),
                on_feedback,
            )?;
            Ok(PinnedDocument::new(pin.clone(), content))
        })
        .collect()
}
