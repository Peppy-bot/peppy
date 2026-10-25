use std::collections::HashMap;
use std::path::PathBuf;

use config::node::{NodeConfig, NodeConfigParser};
use core_node::{ExposureDocumentsError, NodeCacheEntry};
use core_node_api::encoding::{CopyReference, LaunchWord, LauncherOrigin};
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::DeploymentTargets;
use daemon_config::launcher::{
    AlreadyPairedSlots, BindingValidationItem, ClockIncarnations, CopyMembership, Deployment,
    DeploymentSource, ExternallyCoveredSlots, MemberAddressing, PairingValidationItem,
    PeppyLauncher, Placements, PreparedLauncher, ResolvedClocks, ScopeCheck, UnitSelection,
    check_daemon_scopes, instance_restart, resolve_clocks, resolve_restart, restart_request_of,
    validate_link_plan,
};
use daemon_config::repository::EntryOrigin;

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
/// launch the flat file. The one exception is a launcher with a
/// `stack_copies` scope: the flat document keeps the scope and declares no
/// axis, so its launch refuses the scope, whose options no longer run as
/// copies. The resolution report goes to stderr.
///
/// The report's line of each copy writes the machine after each part a word
/// places, as the word wrote it: this command reads no daemon, so it prints
/// `@self` as written, and it shows no machine for a copy the file lists,
/// which `--place` places at launch. The flat launcher places each copy
/// whole, on its name's link.
///
/// The flat plan is then held to the launch checks of daemon scopes, which
/// read the exposure documents and the node manifests alone, so those of the
/// exposure deployments run with a cold nodes cache too, and to the
/// launch-time link rules that need no daemon: slot-key and
/// vacancy legality, and the pairing rules, coverage included, so a launcher that leaves a `zero_or_one` pairing slot neither
/// paired nor vacant fails here instead of minutes later at launch. The
/// node manifests come from this machine's nodes cache, a git-backed one out
/// of the checkout the caches materialized for it; when one is not readable
/// locally the check is skipped and says so, because a partial item list
/// would misreport rules that need both endpoints.
pub fn resolve(
    launcher_config_path: PathBuf,
    words: Vec<LaunchWord>,
    joins: Vec<CopyReference>,
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
    words: &[LaunchWord],
    joins: &[CopyReference],
) -> Result<(String, Vec<String>)> {
    let path = match infer_launcher_origin(launcher_config_path)? {
        LauncherOrigin::Fs(path) => path,
        LauncherOrigin::Repository { name } => {
            core_node::resolve_repo_launcher_path(&name, dirs, &crate::commands::report_as_info)
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
    let copies = CopyMembership::of(composed.copies());
    // Both checks read the node manifests of the plan through one load of
    // this machine's nodes cache.
    let node_cache = core_node::load_node_cache(dirs).map_err(|e| e.to_string());
    check_daemon_scope_rules(
        &prepared,
        &composed.launcher,
        &composed.selection,
        &copies,
        ScopeSources {
            dirs,
            node_cache: &node_cache,
        },
        &mut lines,
    )?;
    check_link_plan(&composed.launcher, &copies, dirs, &node_cache, &mut lines)?;

    let document = json5_pretty::to_string_pretty(&composed.launcher)
        .map_err(|e| Error::ExecutionFailed(format!("cannot serialize the flat launcher: {e}")))?;
    Ok((document, lines))
}

/// Where the checks of daemon scopes read the targets of each deployment:
/// the exposure documents, and the node manifests of the nodes cache.
struct ScopeSources<'a> {
    dirs: &'a PeppyDirs,
    node_cache: &'a std::result::Result<Vec<NodeCacheEntry>, String>,
}

/// Why the targets of one deployment were not read.
enum UnreadTargets {
    /// This machine's caches do not hold what the targets are read from:
    /// the checks of the deployment are skipped, and the report names it.
    NotCached(String),
    /// What the targets are read from is one that the launch would refuse:
    /// the command fails with that refusal.
    Refused(String),
}

/// The targets of `deployment`, at `index` in the flat launcher: those of its
/// exposure documents, or the slots and daemon targets of its node manifest.
fn deployment_targets(
    sources: &ScopeSources<'_>,
    index: usize,
    deployment: &Deployment,
) -> std::result::Result<DeploymentTargets, UnreadTargets> {
    let DeploymentSource::Exposures { exposures } = &deployment.source else {
        let entries = sources.node_cache.as_ref().map_err(|e| {
            UnreadTargets::NotCached(format!(
                "{} (the nodes cache is not readable: {e})",
                deployment.source.label()
            ))
        })?;
        let checked = checked_manifest(sources.dirs, entries, index, deployment)
            .map_err(UnreadTargets::NotCached)?;
        return DeploymentTargets::of_node(&checked.config.manifest)
            .map_err(|refusals| UnreadTargets::Refused(refusals.join("; ")));
    };
    let documents = core_node::resolve_exposure_documents(
        sources.dirs,
        exposures,
        &crate::commands::report_as_info,
    )
    .map_err(|error| match error {
        ExposureDocumentsError::NotCached(reason) => {
            UnreadTargets::NotCached(format!("{} ({reason})", deployment.source.label()))
        }
        ExposureDocumentsError::Refused(reason) => UnreadTargets::Refused(reason),
    })?;
    // A document set the launch would refuse (two sources under one target
    // name, an interface this peppy does not serve) is refused here as it
    // would be there.
    DeploymentTargets::of_exposures(&documents).map_err(|e| UnreadTargets::Refused(e.to_string()))
}

/// Hold the flat plan to the launch checks of daemon scopes: every scope
/// parsed into its interface's type and held against the launcher and the
/// stack `selection` resolves (for `stack_copies`, the trial join of every
/// feature set of every option), every instance with a daemon target on the
/// coordinator, no link to a daemon target, and at most one daemon target
/// of each interface on an instance. They read the exposure documents and
/// the node manifests alone, so the deployments of exposures run them with
/// a cold nodes cache too. A deployment whose documents or manifest this
/// machine's caches do not hold skips its checks, and the report names it,
/// so a clean exit is never mistaken for checked scopes; the checks of every
/// other deployment still run. A document or a manifest the launch would
/// refuse fails the command with that refusal.
fn check_daemon_scope_rules(
    prepared: &PreparedLauncher,
    flat: &PeppyLauncher,
    selection: &UnitSelection,
    copies: &CopyMembership,
    sources: ScopeSources<'_>,
    report: &mut Vec<String>,
) -> Result<()> {
    let mut checked: Vec<(&Deployment, DeploymentTargets)> =
        Vec::with_capacity(flat.deployments.len());
    let mut not_cached_exposures: Vec<String> = Vec::new();
    let mut not_cached_nodes: Vec<String> = Vec::new();
    for (index, deployment) in flat.deployments.iter().enumerate() {
        match deployment_targets(&sources, index, deployment) {
            Ok(targets) => checked.push((deployment, targets)),
            Err(UnreadTargets::NotCached(reason)) => match deployment.source {
                DeploymentSource::Exposures { .. } => not_cached_exposures.push(reason),
                DeploymentSource::Node { .. } => not_cached_nodes.push(reason),
            },
            Err(UnreadTargets::Refused(reason)) => {
                return Err(Error::ExecutionFailed(format!(
                    "deployment {} would be refused at launch: {reason}",
                    deployment.source.label()
                )));
            }
        }
    }
    if !not_cached_exposures.is_empty() {
        report.push(format!(
            "daemon scopes not checked for {} exposure deployment(s) this machine's caches do \
             not hold: {}",
            not_cached_exposures.len(),
            not_cached_exposures.join(", ")
        ));
    }
    if !not_cached_nodes.is_empty() {
        report.push(format!(
            "daemon scopes not checked for {} node deployment(s) whose manifest this machine's \
             caches do not hold: {}",
            not_cached_nodes.len(),
            not_cached_nodes.join(", ")
        ));
    }
    check_daemon_scopes(
        prepared,
        checked
            .iter()
            .map(|(deployment, targets)| (*deployment, targets)),
        copies,
        ScopeCheck::Launch(selection),
    )
    .map_err(|refusals| {
        Error::ExecutionFailed(format!(
            "the flat launcher breaks daemon scope rules a launch would reject:{}",
            daemon_config::format_bulleted(&refusals.0)
        ))
    })?;
    let scoped: usize = checked.iter().map(|(_, served)| served.daemon.len()).sum();
    if scoped > 0 {
        report.push(format!(
            "daemon scopes hold: {scoped} daemon target(s) scoped and placed on the coordinator"
        ));
    }
    Ok(())
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
/// cross-family slot-key and vacancy rules, then the pairing rules, after
/// [`report_clocks_and_restarts`] has reported every instance's clock and
/// policy. Both validators read only the flat launcher and the deployed nodes'
/// manifests, so they run here without a daemon; the contract-binding rules
/// stay launch-only because satisfying them can involve the root node, which
/// only the daemon knows.
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
    node_cache: &std::result::Result<Vec<NodeCacheEntry>, String>,
    report: &mut Vec<String>,
) -> Result<()> {
    let entries = match node_cache {
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
        match checked_manifest(dirs, entries, index, deployment) {
            Ok(manifest) => manifests.push(manifest),
            Err(reason) => unavailable.push(reason),
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
    report_clocks_and_restarts(flat, &clocks, &manifests, report);

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

/// The manifest of the deployment at `index` in the flat launcher, as
/// [`check_link_plan`] judges it, or the report entry that names the
/// deployment and says why its manifest is not readable on this machine.
fn checked_manifest(
    dirs: &PeppyDirs,
    entries: &[NodeCacheEntry],
    index: usize,
    deployment: &Deployment,
) -> std::result::Result<CheckedManifest, String> {
    let (name, tag) = match &deployment.source {
        DeploymentSource::Node { name, tag } => (name.as_str(), tag.as_str()),
        DeploymentSource::Exposures { exposures } => {
            // The built-in server's manifest is derived from the exposures
            // and their contracts, through the same caches and the same
            // derivation a launch uses.
            let plan =
                core_node::resolve_exposure_plan(dirs, exposures, &crate::commands::report_as_info)
                    .map_err(|e| format!("{} ({e})", deployment.source.label()))?;
            return Ok(CheckedManifest {
                name: plan.name.as_str().to_owned(),
                tag: plan.tag,
                index,
                config: plan.config,
                addressing: plan.addressing,
            });
        }
    };
    let id = format!("{name}:{tag}");
    let entry = match core_node::lookup(entries, name, tag) {
        Ok(Some(entry)) => entry,
        Ok(None) => return Err(format!("{id} (not in the nodes cache)")),
        Err(ambiguity) => return Err(format!("{id} ({ambiguity})")),
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
            let Some(checkout) = core_node::materialized_checkout(dirs, repo_url, commit) else {
                return Err(format!(
                    "{id} (no checkout of {commit} on this machine; run `peppy repo refresh`)"
                ));
            };
            checkout.join(path.as_path())
        }
    };
    let config = NodeConfigParser::from_path(&manifest_path).map_err(|e| format!("{id} ({e})"))?;
    // The cache's label and the file's own declaration must agree: a stale
    // entry whose path now holds another node's manifest would otherwise
    // have the plan judged against that node's slot declarations under this
    // one's name.
    if config.manifest.name.as_str() != name || config.manifest.tag != tag {
        return Err(format!(
            "{id} (the manifest at {} declares `{}:{}`)",
            manifest_path.display(),
            config.manifest.name.as_str(),
            config.manifest.tag,
        ));
    }
    Ok(CheckedManifest {
        name: name.to_string(),
        tag: tag.to_string(),
        index,
        config,
        addressing: MemberAddressing::WholeSet,
    })
}

/// Reports the clock, from `clocks`, and the restart policy of every
/// instance of the flat plan, one line each, with the manifests this
/// machine's cache holds, `manifests`. A policy written on the instance needs
/// no manifest; any other needs the node's, and without it the line says the
/// policy is not resolved.
fn report_clocks_and_restarts(
    flat: &PeppyLauncher,
    clocks: &ResolvedClocks,
    manifests: &[CheckedManifest],
    report: &mut Vec<String>,
) {
    let manifest_by_index: HashMap<usize, &CheckedManifest> = manifests
        .iter()
        .map(|checked| (checked.index, checked))
        .collect();
    let root_restart = flat.framework.restart;
    for (index, deployment) in flat.deployments.iter().enumerate() {
        for instance in &deployment.instances {
            let id = instance.instance_id.as_str();
            let binding = clocks.of(id);
            let clock = match binding.domain() {
                None => "clock `wall`".to_owned(),
                Some(domain) if binding.is_publisher() => {
                    format!("publishes clock `{}`", domain.name)
                }
                Some(domain) => format!("clock `{}`", domain.name),
            };
            let request = restart_request_of(root_restart, instance);
            let resolved = instance_restart(request).or_else(|| {
                manifest_by_index
                    .get(&index)
                    .map(|checked| resolve_restart(request, checked.config.framework.restart))
            });
            report.push(match resolved {
                Some(resolved) => format!("{id}: {clock}, restart {resolved}"),
                None => format!("{id}: {clock}, restart not resolved (manifest unavailable)"),
            });
        }
    }
}
