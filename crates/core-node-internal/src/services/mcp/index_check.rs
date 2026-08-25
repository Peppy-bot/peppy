//! Exposure validation for `peppy repo index --check --include-repositories`.

use crate::services::repo::cache::{self as repo_cache, ContractCacheEntry};
use crate::services::repo::index::{
    IndexError, read_repository_index, resolve_declared_item, walk_directory,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::{ManifestFingerprint, RepoItemKind};
use peppy_mcp_catalog::{ResolvedContract, build_exposure_bundle};
use std::path::Path;

/// Everything wrong with one exposure a repository publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureFinding {
    pub id: String,
    pub path: String,
    pub problems: Vec<String>,
}

impl std::fmt::Display for ExposureFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mcp_exposure `{}` ({}):{}",
            self.id,
            self.path,
            self.problems
                .iter()
                .map(|problem| format!("\n    - {problem}"))
                .collect::<String>()
        )
    }
}

/// Validates every exposure the committed index of `root` lists against the
/// contracts it references, resolved through this machine's repository
/// caches, reporting every problem of every exposure at once. A file that
/// declares the exposure schema but does not parse as one (a document
/// naming a public name twice, say) declares no item and is listed by no
/// index, so it is reported here by path rather than passing unseen.
///
/// A contract that cannot be resolved is a problem naming that contract,
/// never a pass: the check is only as good as the caches, so a hub's CI
/// registers and refreshes the contract repository before running it.
pub fn check_repository_exposures(
    root: &Path,
    peppy_dirs: &PeppyDirs,
    on_feedback: &dyn Fn(&str),
) -> Result<Vec<ExposureFinding>, IndexError> {
    let root = std::fs::canonicalize(root).map_err(|source| IndexError::Io {
        path: root.display().to_string(),
        source,
    })?;
    let committed = read_repository_index(&root)?;
    let contract_entries = repo_cache::load_repo_cache::<ContractCacheEntry>(peppy_dirs)
        .map_err(|e| IndexError::Unreadable(format!("failed to load the contract cache: {e}")))?;

    let mut findings: Vec<ExposureFinding> = walk_directory(&root, &[])
        .malformed
        .into_iter()
        .filter(|document| document.kind == RepoItemKind::McpExposure)
        .map(|document| ExposureFinding {
            id: document.path.clone(),
            path: document.path,
            problems: vec![format!(
                "does not parse as an exposure: {}",
                document.reason
            )],
        })
        .collect();
    for item in committed
        .declared_items()
        .filter(|item| item.kind == RepoItemKind::McpExposure)
    {
        let id = match item.tag {
            Some(tag) => format!("{}:{tag}", item.name),
            None => item.name.to_string(),
        };
        let path = item.path.as_str().to_owned();
        let problems = match resolve_declared_item(&root, &item) {
            Err(detail) => vec![format!("{path} {detail}")],
            Ok(bytes) => match std::str::from_utf8(&bytes) {
                Err(e) => vec![format!("{path} is not UTF-8: {e}")],
                Ok(content) => match PeppyMcpExposureParser::from_content(content) {
                    Err(e) => vec![format!("{path} does not parse: {e}")],
                    Ok(exposure) => {
                        exposure_problems(&exposure, peppy_dirs, &contract_entries, on_feedback)
                    }
                },
            },
        };
        if !problems.is_empty() {
            findings.push(ExposureFinding { id, path, problems });
        }
    }
    findings.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(findings)
}

/// One exposure's problems: every contract it references that the caches
/// cannot resolve, then every validation violation against the ones they
/// can.
fn exposure_problems(
    exposure: &daemon_config::mcp_exposure::McpExposure,
    peppy_dirs: &PeppyDirs,
    contract_entries: &[ContractCacheEntry],
    on_feedback: &dyn Fn(&str),
) -> Vec<String> {
    let mut problems = Vec::new();
    // Identity -> (fingerprint, parsed document); resolved once per identity.
    let mut resolved: Vec<(
        String,
        String,
        ManifestFingerprint,
        daemon_config::contract::PeppyContract,
    )> = Vec::new();
    for target in exposure.targets.values() {
        let reference = &target.contract;
        let name = reference.name.as_str();
        if resolved
            .iter()
            .any(|(n, t, _, _)| n == name && t == &reference.tag)
        {
            continue;
        }
        match repo_cache::resolve_cached_doc_entry(
            peppy_dirs,
            contract_entries,
            name,
            &reference.tag,
            reference.sha256.as_ref().map(ManifestFingerprint::as_str),
            on_feedback,
        ) {
            Err(detail) => problems.push(detail),
            Ok((entry, bytes)) => match std::str::from_utf8(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|content| {
                    PeppyContractParser::from_content(content).map_err(|e| e.to_string())
                }) {
                Err(detail) => problems.push(format!(
                    "contract `{name}:{}` does not parse: {detail}",
                    reference.tag
                )),
                Ok(document) => resolved.push((
                    name.to_owned(),
                    reference.tag.clone(),
                    entry.sha256.clone(),
                    document,
                )),
            },
        }
    }
    if !problems.is_empty() {
        // Validation against a partial contract set would report every
        // member of the missing contract as unknown, restating the one
        // problem above as many.
        return problems;
    }
    let contracts: Vec<ResolvedContract<'_>> = resolved
        .iter()
        .map(|(name, tag, sha256, document)| ResolvedContract {
            name,
            tag,
            sha256,
            topics: &document.interfaces.topics,
            services: &document.interfaces.services,
            actions: &document.interfaces.actions,
        })
        .collect();
    if let Err(error) = build_exposure_bundle(exposure, &contracts) {
        problems.extend(error.violations);
    }
    problems
}
