//! Exposure validation for `peppy repo index --check --include-repositories`.

use super::resolve::resolve_cached_document;
use crate::services::repo::cache::{self as repo_cache, ContractCacheEntry};
use crate::services::repo::index::{
    IndexError, read_repository_index, resolve_declared_item, walk_directory,
};
use daemon_config::consts::PeppyDirs;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_deployment::PinnedContract;
use daemon_config::mcp_exposure::{McpExposure, PeppyMcpExposureParser, PinnedContractRef};
use daemon_config::repository::{ManifestFingerprint, RepoItemKind};
use peppy_mcp_catalog::build_exposure_bundle;
use std::collections::{BTreeMap, BTreeSet};
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
    let mut contracts = Contracts {
        peppy_dirs,
        entries: &contract_entries,
        on_feedback,
        resolved: BTreeMap::new(),
    };

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
                    Ok(exposure) => exposure_problems(&exposure, &mut contracts),
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

/// The identity a reference resolves through the cache, with the author's
/// pin: two references at different pins may resolve to different bytes.
type ContractKey = (String, String, Option<String>);

/// The contracts the exposures of one check reference, each resolved
/// through the caches once, however many exposures name it.
struct Contracts<'a> {
    peppy_dirs: &'a PeppyDirs,
    entries: &'a [ContractCacheEntry],
    on_feedback: &'a dyn Fn(&str),
    resolved: BTreeMap<ContractKey, Result<PinnedContract, String>>,
}

impl Contracts<'_> {
    fn key(reference: &PinnedContractRef) -> ContractKey {
        (
            reference.name.as_str().to_owned(),
            reference.tag.clone(),
            reference.sha256.as_ref().map(ToString::to_string),
        )
    }

    /// The contract a reference names, resolved on first use.
    fn get(&mut self, reference: &PinnedContractRef) -> &Result<PinnedContract, String> {
        let key = Self::key(reference);
        if !self.resolved.contains_key(&key) {
            let resolved = self.resolve(reference);
            self.resolved.insert(key.clone(), resolved);
        }
        &self.resolved[&key]
    }

    fn resolve(&self, reference: &PinnedContractRef) -> Result<PinnedContract, String> {
        let name = reference.name.as_str();
        let tag = reference.tag.as_str();
        let document = resolve_cached_document(
            self.peppy_dirs,
            self.entries,
            name,
            tag,
            reference.sha256.as_ref().map(ManifestFingerprint::as_str),
            self.on_feedback,
        )?;
        let parsed = PeppyContractParser::from_content(&document.content)
            .map_err(|e| format!("contract `{name}:{tag}` does not parse: {e}"))?;
        Ok(PinnedContract {
            pin: document.pin,
            document: parsed,
        })
    }
}

/// One exposure's problems: every contract it references that the caches
/// cannot resolve, then every validation violation against the ones they
/// can.
fn exposure_problems(exposure: &McpExposure, contracts: &mut Contracts<'_>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut resolved: Vec<&PinnedContract> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    // Resolve first, then borrow: every reference goes through the memo
    // before any resolved contract is held by reference.
    for target in exposure.targets.values() {
        contracts.get(&target.contract);
    }
    for target in exposure.targets.values() {
        let reference = &target.contract;
        if !seen.insert((reference.name.as_str().to_owned(), reference.tag.clone())) {
            continue;
        }
        match &contracts.resolved[&Contracts::key(reference)] {
            Err(detail) => problems.push(detail.clone()),
            Ok(contract) => resolved.push(contract),
        }
    }
    if !problems.is_empty() {
        // Validation against a partial contract set would report every
        // member of the missing contract as unknown, restating the one
        // problem above as many.
        return problems;
    }
    let contracts: Vec<_> = resolved
        .iter()
        .map(|contract| contract.resolved())
        .collect();
    if let Err(error) = build_exposure_bundle(exposure, &contracts) {
        problems.extend(error.violations);
    }
    problems
}
