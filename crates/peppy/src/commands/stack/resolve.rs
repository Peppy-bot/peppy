use std::path::PathBuf;

use config::node::{NodeConfig, NodeConfigParser};
use core_node_api::encoding::{LaunchJoin, LauncherOrigin};
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::{
    AlreadyPairedSlots, BindingValidationItem, ClockIncarnations, CopyMembership, DeploymentSource,
    ExternallyCoveredSlots, MemberAddressing, PairingValidationItem, PeppyLauncher, Placements,
    PreparedLauncher, resolve_clocks, validate_link_plan,
};
use daemon_config::repository::EntryOrigin;
use tracing::info;

use super::launch::{infer_launcher_origin, parse_launcher_file};
use crate::error::{Error, Result};

/// The machine a preview places every instance on.
///
/// A domain's identity names the machine its publisher runs on, and this
/// command talks to no daemon, so it has none to name. Every comparison it
/// makes is between instances of the one plan, where a single machine makes
/// them read exactly as they will at launch. The report lines name a domain by
/// name alone; a clock refusal names the whole identity, so this name reaches
/// an operator there.
const PREVIEW_CORE_NODE: &str = "cn-preview";

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
/// included, so a launcher that leaves a `zero_or_one` pairing slot neither
/// paired nor vacant fails here instead of minutes later at launch. The
/// node manifests come from this machine's nodes cache, a git-backed one out
/// of the checkout the caches materialized for it; when one is not readable
/// locally the check is skipped and says so, because a partial item list
/// would misreport rules that need both endpoints.
pub fn resolve(
    launcher_config_path: PathBuf,
    words: Vec<String>,
    joins: Vec<LaunchJoin>,
) -> Result<()> {
    let (document, report) =
        resolve_rendered(&PeppyDirs::default(), launcher_config_path, &words, &joins)?;
    for line in report {
        eprintln!("{line}");
    }
    println!("{document}");
    Ok(())
}

/// The resolve command's whole verdict in printable form: the flattened
/// document for stdout and the report lines for stderr, validated against
/// the `PeppyDirs` it is handed, so a test can read both halves and point
/// the check at a root it wrote. Link-rule violations are an `Err`, exactly
/// as the launch they predict would be.
pub fn resolve_rendered(
    dirs: &PeppyDirs,
    launcher_config_path: PathBuf,
    words: &[String],
    joins: &[LaunchJoin],
) -> Result<(String, Vec<String>)> {
    let path = match infer_launcher_origin(launcher_config_path)? {
        LauncherOrigin::Fs(path) => path,
        LauncherOrigin::Repository { name } => {
            core_node::resolve_repo_launcher_path(&name, dirs, &|message: &str| info!("{message}"))
                .map_err(Error::ExecutionFailed)?
        }
    };

    let parsed = parse_launcher_file(&path)?;
    let prepared = PreparedLauncher::load(&parsed, &path)
        .map_err(|e| Error::ExecutionFailed(e.to_string()))?;
    let composed = prepared
        .launch(words, joins)
        .map_err(|e| Error::ExecutionFailed(e.to_string()))?;
    let mut lines = composed.report.render_lines();
    check_link_plan(
        &composed.launcher,
        &CopyMembership::of(composed.copies()),
        dirs,
        &mut lines,
    )?;

    let document = json5_pretty::to_string_pretty(&composed.launcher)
        .map_err(|e| Error::ExecutionFailed(format!("cannot serialize the flat launcher: {e}")))?;
    Ok((document, lines))
}

/// One deployment's manifest as [`check_link_plan`] resolved it: the
/// identity it declares, the deployment it came from, and how its node reads
/// the sets its slots hold.
struct CheckedManifest {
    name: String,
    tag: String,
    /// The deployment's position in the flat launcher, which carries the
    /// instances the manifest is judged against.
    index: usize,
    config: NodeConfig,
    addressing: MemberAddressing,
}

/// Hold the flat plan to the launch-time link rules a client can check: the
/// cross-family slot-key and vacancy rules, then the pairing rules. Both
/// validators read only the flat launcher and the deployed nodes' manifests,
/// so they run here without a daemon; the contract-binding rules stay
/// launch-only because satisfying them can involve the root node, which only
/// the daemon knows.
///
/// Manifests come from the nodes cache, a git-backed entry's from the
/// checkout the caches materialized for it. The check runs only when every
/// deployed node's manifest is readable on this machine: the pairing rules
/// judge links by both endpoints' declarations, so validating a partial item
/// list would trade missed errors for false ones. When something is missing
/// the report says which check was skipped and why, so a clean exit is never
/// mistaken for a validated plan.
fn check_link_plan(
    flat: &PeppyLauncher,
    copies: &CopyMembership,
    dirs: &PeppyDirs,
    report: &mut Vec<String>,
) -> Result<()> {
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
    let mut manifests: Vec<CheckedManifest> = Vec::new();
    for (index, deployment) in flat.deployments.iter().enumerate() {
        let (name, tag) = match &deployment.source {
            DeploymentSource::Node { name, tag } => (name.as_str(), tag.as_str()),
            DeploymentSource::Exposures { exposures } => {
                // The built-in server's manifest is derived from the
                // exposures and their contracts, through the same caches
                // and the same derivation a launch uses.
                match core_node::resolve_exposure_plan(dirs, exposures, &|message: &str| {
                    info!("{message}")
                }) {
                    Ok(plan) => {
                        manifests.push(CheckedManifest {
                            name: plan.name.as_str().to_owned(),
                            tag: plan.tag,
                            index,
                            config: plan.config,
                            addressing: plan.addressing,
                        });
                    }
                    Err(e) => unavailable.push(format!("{} ({e})", deployment.source.label())),
                }
                continue;
            }
        };
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
            EntryOrigin::Git {
                repo_url,
                commit,
                path,
                ..
            } => {
                // Read out of the checkout the caches already hold: `repo
                // refresh` clones every repository it indexes and hands that
                // clone to the checkout cache, so the tree behind an entry it
                // wrote is on this machine. A commit nothing has materialized
                // is named instead of fetched, because this command never
                // reaches the network.
                match core_node::materialized_checkout(dirs, repo_url, commit) {
                    Some(checkout) => checkout.join(path.as_path()),
                    None => {
                        unavailable.push(format!(
                            "{id} (no checkout of {commit} on this machine; run `peppy repo refresh`)"
                        ));
                        continue;
                    }
                }
            }
        };
        match NodeConfigParser::from_path(&manifest_path) {
            // The cache's label and the file's own declaration must agree:
            // a stale entry whose path now holds another node's manifest
            // would otherwise have the plan judged against that node's slot
            // declarations under this one's name.
            Ok(config) if config.manifest.name.as_str() == name && config.manifest.tag == tag => {
                manifests.push(CheckedManifest {
                    name: name.to_string(),
                    tag: tag.to_string(),
                    index,
                    config,
                    addressing: MemberAddressing::WholeSet,
                });
            }
            Ok(config) => {
                unavailable.push(format!(
                    "{id} (the manifest at {} declares `{}:{}`)",
                    manifest_path.display(),
                    config.manifest.name.as_str(),
                    config.manifest.tag,
                ));
            }
            Err(e) => {
                unavailable.push(format!("{id} ({e})"));
            }
        }
    }
    // The clock rules read no manifest, so they hold whether or not the
    // nodes are in this machine's cache.
    let placements = Placements::all_on(
        config::runtime::CoreNodeName::new(PREVIEW_CORE_NODE)
            .expect("the preview machine name is a valid core node name"),
    );
    let clocks = match resolve_clocks(flat, &placements, &ClockIncarnations::new()) {
        Ok(clocks) => clocks,
        Err(errors) => {
            let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
            return Err(Error::ExecutionFailed(format!(
                "the flat launcher breaks clock rules a launch would reject:{}",
                daemon_config::format_bulleted(&rendered)
            )));
        }
    };
    for instance in flat
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
    {
        let id = instance.instance_id.as_str();
        let binding = clocks.of(id);
        report.push(match binding.domain() {
            None => format!("{id}: clock `wall`"),
            Some(domain) if binding.is_publisher() => {
                format!("{id}: publishes clock `{}`", domain.name)
            }
            Some(domain) => format!("{id}: clock `{}`", domain.name),
        });
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
        .map(|checked| BindingValidationItem {
            node_name: &checked.name,
            node_tag: &checked.tag,
            instances: &flat.deployments[checked.index].instances,
            depends_on: checked.config.manifest.depends_on.as_ref(),
            implements: &checked.config.manifest.implements,
            addressing: &checked.addressing,
        })
        .collect();
    let pairing_items: Vec<PairingValidationItem<'_>> = manifests
        .iter()
        .map(|checked| PairingValidationItem {
            node_name: &checked.name,
            node_tag: &checked.tag,
            instances: &flat.deployments[checked.index].instances,
            pairing_deps: checked
                .config
                .manifest
                .depends_on
                .as_ref()
                .map(|d| d.pairings.as_slice())
                .unwrap_or_default(),
            observer_deps: checked
                .config
                .manifest
                .depends_on
                .as_ref()
                .map(|d| d.pairing_observers.as_slice())
                .unwrap_or_default(),
            preexisting: false,
        })
        .collect();
    // A preview starts nothing, so no slot is already claimed and none is
    // covered outside the plan this reads.
    let validated = validate_link_plan(
        &binding_items,
        &pairing_items,
        &AlreadyPairedSlots::new(),
        &ExternallyCoveredSlots::new(),
        &placements,
        copies,
        &clocks,
    );
    if validated.errors.is_empty() {
        report.push(format!(
            "link rules hold: slot keys, vacancies, pairing coverage, observation sources and \
             clock agreement checked over {} node manifest(s)",
            manifests.len()
        ));
        return Ok(());
    }
    let rendered: Vec<String> = validated.errors.iter().map(ToString::to_string).collect();
    Err(Error::ExecutionFailed(format!(
        "the flat launcher breaks link rules a launch would reject:{}",
        daemon_config::format_bulleted(&rendered)
    )))
}
