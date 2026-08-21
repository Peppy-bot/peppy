use std::path::PathBuf;
use std::sync::Arc;

use config::node::{NodeConfig, NodeConfigParser};
use core_node_api::encoding::LauncherOrigin;
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::{
    AlreadyPairedSlots, BindingValidationItem, ExternallyCoveredSlots, PairingValidationItem,
    PeppyLauncher, PeppyLauncherParser, compose, validate_link_slots, validate_pairings,
};
use daemon_config::repository::EntryOrigin;
use tracing::info;

use super::launch::infer_launcher_origin;
use crate::context::AppContext;
use crate::error::{Error, Result};

/// `peppy stack resolve <name|path> [--with ...]`: print the flat launcher
/// a composed launch would run, and the report of what the selection did.
///
/// Needs no running stack. A filesystem input is read where it stands; a
/// repository name resolves through this machine's launcher cache, exactly
/// as a launch would, minus the goal. The flattened `launcher/v1` document
/// goes to stdout, so it doubles as the escape hatch: flatten, hand-edit,
/// launch the flat file. The resolution report goes to stderr.
///
/// The flat plan is then held to the launch-time link rules that need no
/// daemon: slot-key and vacancy legality, and the pairing rules, coverage
/// included, so a launcher that leaves an optional pairing slot neither
/// paired nor vacant fails here instead of minutes later at launch. The
/// node manifests come from this machine's nodes cache; when one is not
/// readable locally the check is skipped and says so, because a partial
/// item list would misreport rules that need both endpoints.
pub fn resolve(
    _ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    with: Vec<String>,
) -> Result<()> {
    let (document, report) = resolve_rendered(&PeppyDirs::default(), launcher_config_path, &with)?;
    for line in report {
        eprintln!("{line}");
    }
    println!("{document}");
    Ok(())
}

/// The resolve command's whole verdict in printable form: the flattened
/// document for stdout and the report lines for stderr. Split from
/// [`resolve`] so a test can read the output instead of capturing stdout,
/// and handed its `PeppyDirs` so a test validates against a root it wrote
/// rather than whatever this machine's caches hold. Link-rule violations
/// are an `Err`, exactly as the launch they predict would be.
pub fn resolve_rendered(
    dirs: &PeppyDirs,
    launcher_config_path: PathBuf,
    with: &[String],
) -> Result<(String, Vec<String>)> {
    let path = match infer_launcher_origin(launcher_config_path)? {
        LauncherOrigin::Fs(path) => path,
        LauncherOrigin::Repository { name } => {
            core_node::resolve_repo_launcher_path(&name, dirs, &|message: &str| info!("{message}"))
                .map_err(Error::ExecutionFailed)?
        }
    };

    let parsed = PeppyLauncherParser::from_path(&path).map_err(Error::DaemonConfig)?;
    let (flat, report) =
        compose(&parsed, &path, with).map_err(|e| Error::ExecutionFailed(e.to_string()))?;

    let mut lines = report.render_lines();
    check_link_plan(&flat, dirs, &mut lines)?;

    let document = json5_pretty::to_string_pretty(&flat)
        .map_err(|e| Error::ExecutionFailed(format!("cannot serialize the flat launcher: {e}")))?;
    Ok((document, lines))
}

/// Hold the flat plan to the launch-time link rules a client can check: the
/// cross-family slot-key and vacancy rules, then the pairing rules. Both
/// validators read only the flat launcher and the deployed nodes' manifests,
/// so they run here without a daemon; the contract-binding rules stay
/// launch-only because satisfying them can involve the root node, which only
/// the daemon knows.
///
/// Manifests come from the nodes cache. The check runs only when every
/// deployed node's manifest is readable on this machine: the pairing rules
/// judge links by both endpoints' declarations, so validating a partial item
/// list would trade missed errors for false ones. When something is missing
/// the report says which check was skipped and why, so a clean exit is never
/// mistaken for a validated plan.
fn check_link_plan(flat: &PeppyLauncher, dirs: &PeppyDirs, report: &mut Vec<String>) -> Result<()> {
    let entries = match core_node::load_node_cache(dirs) {
        Ok(entries) if !entries.is_empty() => entries,
        Ok(_) => {
            report.push(
                "link rules not checked: the nodes cache is empty; run `peppy repo refresh`"
                    .to_string(),
            );
            return Ok(());
        }
        Err(e) => {
            report.push(format!(
                "link rules not checked: the nodes cache is not readable ({e}); run `peppy repo refresh`"
            ));
            return Ok(());
        }
    };

    let mut unavailable: Vec<String> = Vec::new();
    let mut manifests: Vec<(String, String, usize, NodeConfig)> = Vec::new();
    for (index, deployment) in flat.deployments.iter().enumerate() {
        let name = deployment.source.name.as_str();
        let tag = deployment.source.tag.as_str();
        let id = format!("{name}:{tag}");
        let entry = match core_node::lookup(&entries, name, tag) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                unavailable.push(format!("{id} (not in the nodes cache)"));
                continue;
            }
            Err(ambiguity) => {
                unavailable.push(format!("{id} ({ambiguity})"));
                continue;
            }
        };
        let manifest_path = match &entry.origin {
            EntryOrigin::Fs { path } => path.clone(),
            EntryOrigin::Git { .. } => {
                // A git-backed manifest may need a fetch to read, which this
                // command must not do; the launch that follows will.
                unavailable.push(format!(
                    "{id} (a git entry's manifest needs materializing to read)"
                ));
                continue;
            }
        };
        match NodeConfigParser::from_path(&manifest_path) {
            Ok(config) => manifests.push((name.to_string(), tag.to_string(), index, config)),
            Err(e) => {
                unavailable.push(format!("{id} ({e})"));
            }
        }
    }
    if !unavailable.is_empty() {
        report.push(format!(
            "link rules not checked, {} manifest(s) unavailable: {}",
            unavailable.len(),
            unavailable.join(", ")
        ));
        return Ok(());
    }

    let binding_items: Vec<BindingValidationItem<'_>> = manifests
        .iter()
        .map(|(name, tag, index, config)| BindingValidationItem {
            node_name: name,
            node_tag: tag,
            instances: &flat.deployments[*index].instances,
            depends_on: config.manifest.depends_on.as_ref(),
            implements: &config.manifest.implements,
        })
        .collect();
    let mut errors = validate_link_slots(&binding_items);
    if errors.is_empty() {
        let pairing_items: Vec<PairingValidationItem<'_>> = manifests
            .iter()
            .map(|(name, tag, index, config)| PairingValidationItem {
                node_name: name,
                node_tag: tag,
                instances: &flat.deployments[*index].instances,
                pairing_deps: config
                    .manifest
                    .depends_on
                    .as_ref()
                    .map(|d| d.pairings.as_slice())
                    .unwrap_or_default(),
                observer_deps: config
                    .manifest
                    .depends_on
                    .as_ref()
                    .map(|d| d.pairing_observers.as_slice())
                    .unwrap_or_default(),
                preexisting: false,
            })
            .collect();
        errors = validate_pairings(
            &pairing_items,
            &AlreadyPairedSlots::new(),
            &ExternallyCoveredSlots::new(),
        )
        .errors;
    }
    if errors.is_empty() {
        report.push(format!(
            "link rules hold: slot keys, vacancies and pairing coverage checked over {} node manifest(s)",
            manifests.len()
        ));
        return Ok(());
    }
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    Err(Error::ExecutionFailed(format!(
        "the flat launcher breaks link rules a launch would reject:{}",
        daemon_config::format_bulleted(&rendered)
    )))
}
