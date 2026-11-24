use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use config::node::{NodeConfig, NodeConfigParser};
use config::runtime::CoreNodeTarget;
use core_node::{ExposureDocumentsError, NodeCacheEntry};
use core_node_api::encoding::{CopyReference, LaunchWord, LauncherOrigin};
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::DeploymentTargets;
use daemon_config::launcher::{
    AlreadyPairedSlots, BindingValidationItem, ClockIncarnations, ComposedLaunch, CopyMembership,
    Deployment, DeploymentInstance, DeploymentSource, ExternallyCoveredSlots, MemberAddressing,
    PairingValidationItem, PeppyLauncher, Placements, PreparedLauncher, ResolvedClocks, ScopeCheck,
    Starter, UnitSelection, VariableNeeds, check_daemon_scopes, instance_restart, resolve_clocks,
    resolve_restart, restart_request_of, validate_link_plan,
};
use daemon_config::repository::EntryOrigin;
use serde::{Deserialize, Serialize};

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

/// What one `peppy stack resolve` resolves: the one selection that
/// `--with` and `--join` write, or each selection that a `--selections`
/// file lists.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResolveInput {
    One(LaunchSelection),
    /// The path of the selections file, or `-` for stdin.
    Many(PathBuf),
}

impl ResolveInput {
    /// The input that the command line gives. `--selections` takes the place
    /// of `--with` and `--join`, so a word or a copy next to it is refused.
    pub(super) fn parse(
        words: Vec<LaunchWord>,
        joins: Vec<CopyReference>,
        selections: Option<PathBuf>,
    ) -> Result<Self> {
        let Some(selections) = selections else {
            return Ok(Self::One(LaunchSelection { words, joins }));
        };
        if !words.is_empty() || !joins.is_empty() {
            return Err(Error::ExecutionFailed(
                "`--selections` takes the place of `--with` and `--join`, so give it without them"
                    .to_owned(),
            ));
        }
        Ok(Self::Many(selections))
    }
}

/// `peppy stack resolve`: resolve the one selection of `input`, or each
/// selection that its file lists.
pub(super) fn resolve(launcher_config_path: PathBuf, input: ResolveInput) -> Result<()> {
    match input {
        ResolveInput::One(selection) => resolve_one(launcher_config_path, &selection),
        ResolveInput::Many(selections) => resolve_selections(launcher_config_path, &selections),
    }
}

/// `peppy stack resolve <name|path> [--with ...]`: print the flat launcher
/// a composed launch would run, and the report of what the selection did.
///
/// Needs no running stack. A filesystem input is read where it stands; a
/// repository name resolves through this machine's launcher cache, exactly
/// as a launch would, minus the goal. The flattened `launcher/v1` document
/// goes to stdout, so it doubles as the escape hatch: flatten, hand-edit,
/// launch the flat file. The one exception is a launcher with a
/// `robot_membership` scope: the flat document keeps the scope and declares no
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
/// exposure deployments run with a cold nodes cache too. The report then
/// names the launch variables that the launch needs and each instance with
/// launch variables that the launch can place off the coordinator (see
/// [`report_launch_variables`]). The plan is then held to the
/// launch-time link rules that need no daemon: slot-key and
/// vacancy legality, and the pairing rules, coverage included, so a launcher that leaves a `zero_or_one` pairing slot neither
/// paired nor vacant fails here instead of minutes later at launch. The
/// node manifests come from this machine's nodes cache, a git-backed one out
/// of the checkout the caches materialized for it; when one is not readable
/// locally the check is skipped and says so, because a partial item list
/// would misreport rules that need both endpoints.
fn resolve_one(launcher_config_path: PathBuf, selection: &LaunchSelection) -> Result<()> {
    let (document, report) = resolve_rendered(
        &PeppyDirs::default(),
        launcher_config_path,
        &selection.words,
        &selection.joins,
    )?;
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
    let resolved = LauncherResolver::load(dirs, launcher_config_path)?.resolve(words, joins)?;
    let document =
        json5_pretty::to_string_pretty(&resolved.launcher).map_err(flat_launcher_not_serialized)?;
    Ok((document, resolved.report))
}

/// The failure of a selection whose flat launcher does not serialize.
fn flat_launcher_not_serialized(reason: impl std::fmt::Display) -> Error {
    Error::ExecutionFailed(format!("cannot serialize the flat launcher: {reason}"))
}

/// `peppy stack resolve <name|path> --selections <file>`: resolve every
/// selection that the file lists, and print one JSON record on a line of
/// stdout for each, in the order of the file.
///
/// The launcher and its fragments are read one time, and the nodes cache is
/// read one time. The manifest and the targets of each deployment source are
/// read at the first selection that deploys it and kept for the selections
/// after it (see [`PlanSources`]). So the records are the verdicts that
/// `peppy stack resolve` gives for each selection alone, at a fraction of
/// the cost of one command for each selection.
///
/// `selections` is the path of the file, or `-` for stdin. See
/// [`parse_selections`] for a line of it and [`SelectionRecord`] for a
/// record.
fn resolve_selections(launcher_config_path: PathBuf, selections: &Path) -> Result<()> {
    let selections = parse_selections(&read_selections(selections)?)?;
    let mut records = std::io::BufWriter::new(std::io::stdout().lock());
    write_selection_records(
        &PeppyDirs::default(),
        launcher_config_path,
        &selections,
        &mut records,
    )
}

/// The text of the `--selections` input: the file at `path`, or stdin where
/// the path is `-`.
fn read_selections(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut text)
            .map_err(|e| {
                Error::ExecutionFailed(format!("cannot read the selections from stdin: {e}"))
            })?;
        return Ok(text);
    }
    std::fs::read_to_string(path).map_err(|e| {
        Error::ExecutionFailed(format!(
            "cannot read the selections file {}: {e}",
            path.display()
        ))
    })
}

/// One selection of a launcher: the `--with` words and the `--join` copies
/// of one `peppy stack resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSelection {
    pub words: Vec<LaunchWord>,
    pub joins: Vec<CopyReference>,
}

/// One line of the `--selections` input, before its words are parsed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionLine {
    #[serde(default)]
    with: Vec<String>,
    #[serde(default)]
    join: Vec<String>,
}

/// The selections that `input` lists, one on each line: a JSON object whose
/// `with` is a list of `--with` words and whose `join` is a list of `--join`
/// copies, each one word or one copy as the command line takes it. Each
/// member is optional, so `{}` is the launch with no word.
///
/// A line that is not such an object fails the whole input and is named by
/// its number, an empty line too: a record answers the selection of the same
/// position, so no line is skipped.
pub fn parse_selections(input: &str) -> Result<Vec<LaunchSelection>> {
    input
        .lines()
        .enumerate()
        .map(|(index, line)| {
            parse_selection(line).map_err(|reason| {
                Error::ExecutionFailed(format!("selections line {}: {reason}", index + 1))
            })
        })
        .collect()
}

fn parse_selection(line: &str) -> std::result::Result<LaunchSelection, String> {
    if line.trim().is_empty() {
        return Err("the line is empty; write `{}` for the launch with no word".to_owned());
    }
    let SelectionLine { with, join } = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let words = with
        .iter()
        .map(|word| {
            word.parse::<LaunchWord>()
                .map_err(|e| format!("`with` word `{word}`: {e}"))
        })
        .collect::<std::result::Result<_, _>>()?;
    let joins = join
        .iter()
        .map(|copy| {
            copy.parse::<CopyReference>()
                .map_err(|e| format!("`join` copy `{copy}`: {e}"))
        })
        .collect::<std::result::Result<_, _>>()?;
    Ok(LaunchSelection { words, joins })
}

/// What `peppy stack resolve --selections` prints for one selection: the
/// flat launcher as JSON with the lines of the resolution report, or the
/// message that `peppy stack resolve` fails with for that selection.
///
/// `L` is the flat launcher, a [`PeppyLauncher`] in each record that the
/// command writes.
#[derive(Serialize)]
#[serde(untagged)]
enum SelectionRecord<'a, L> {
    Resolved {
        launcher: &'a L,
        report: &'a [String],
    },
    Failed {
        error: String,
    },
}

impl<L: Serialize> SelectionRecord<'_, L> {
    /// The record as one line of JSON, with its newline. A flat launcher
    /// that does not serialize as JSON gives the `error` record of its
    /// selection, so each line holds one whole record.
    fn to_line(&self) -> Vec<u8> {
        let mut line = serde_json::to_vec(self).unwrap_or_else(|e| {
            let failed = SelectionRecord::<L>::Failed {
                error: flat_launcher_not_serialized(e).to_string(),
            };
            serde_json::to_vec(&failed).expect("a record of one message serializes")
        });
        line.push(b'\n');
        line
    }
}

/// Resolve each of `selections` over the launcher and write its
/// [`SelectionRecord`] to `records`, one JSON object on a line, in the order
/// given, against the `PeppyDirs` it is handed, so a test can point the
/// checks at a root it wrote.
///
/// A launcher that does not load fails the command and writes no record. A
/// selection that does not resolve fails its own record alone.
pub fn write_selection_records(
    dirs: &PeppyDirs,
    launcher_config_path: PathBuf,
    selections: &[LaunchSelection],
    records: &mut impl Write,
) -> Result<()> {
    let resolver = LauncherResolver::load(dirs, launcher_config_path)?;
    for selection in selections {
        let resolved = resolver.resolve(&selection.words, &selection.joins);
        let record = match &resolved {
            Ok(resolved) => SelectionRecord::Resolved {
                launcher: &resolved.launcher,
                report: &resolved.report,
            },
            Err(error) => SelectionRecord::Failed {
                error: error.to_string(),
            },
        };
        records.write_all(&record.to_line())?;
    }
    records.flush()?;
    Ok(())
}

/// The flat launcher of one selection and the lines of its resolution
/// report.
#[derive(Debug)]
pub struct ResolvedLaunch {
    pub launcher: PeppyLauncher,
    pub report: Vec<String>,
}

/// One launcher held ready to resolve any number of selections: its file and
/// its fragments read one time, and what the checks read from this machine's
/// caches for a deployment kept from the first selection that deploys it.
#[derive(Debug)]
pub struct LauncherResolver<'a> {
    prepared: PreparedLauncher,
    sources: PlanSources<'a>,
}

impl<'a> LauncherResolver<'a> {
    /// Read the launcher that `launcher_config_path` names: a filesystem
    /// input where it stands, a repository name through the launcher cache
    /// of `dirs`.
    pub fn load(dirs: &'a PeppyDirs, launcher_config_path: PathBuf) -> Result<Self> {
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
        Ok(Self {
            prepared,
            sources: PlanSources::new(dirs),
        })
    }

    /// Compose the launch of one selection and hold its flat plan to the
    /// checks of the command.
    pub fn resolve(&self, words: &[LaunchWord], joins: &[CopyReference]) -> Result<ResolvedLaunch> {
        let composed = self
            .prepared
            .launch(words, joins)
            .map_err(|e| Error::ExecutionFailed(e.to_string()))?;
        let mut report = composed.report.render_lines();
        let copies = CopyMembership::of(composed.copies());
        let scoped_needs = check_daemon_scope_rules(
            &self.prepared,
            &composed.launcher,
            &composed.selection,
            &copies,
            &self.sources,
            &mut report,
        )?;
        report_launch_variables(&composed, joins, scoped_needs, &mut report);
        check_link_plan(&composed.launcher, &copies, &self.sources, &mut report)?;
        Ok(ResolvedLaunch {
            launcher: composed.launcher,
            report,
        })
    }
}

/// What a deployment source gives when it is read: `Ok` with the value, or
/// `Err` with why this machine did not give it.
type SourceRead<T, E> = std::result::Result<Rc<T>, E>;

/// Each deployment source that was read, with what the read gave.
type ReadSources<T, E> = RefCell<HashMap<DeploymentSource, SourceRead<T, E>>>;

/// What `read` gives for `source`, read at the first request for the source
/// and kept in `known` for each later one.
fn read_once<T, E: Clone>(
    known: &ReadSources<T, E>,
    source: &DeploymentSource,
    read: impl FnOnce() -> std::result::Result<T, E>,
) -> SourceRead<T, E> {
    if let Some(kept) = known.borrow().get(source) {
        return kept.clone();
    }
    let fresh = read().map(Rc::new);
    known.borrow_mut().insert(source.clone(), fresh.clone());
    fresh
}

/// Where the checks of a flat plan read what this machine's caches hold of
/// each deployment: the nodes cache, the node manifests and the exposure
/// documents. The nodes cache is read one time. The manifest and the targets
/// of a deployment source are each read at the first plan that needs them
/// and kept for each later plan, so every selection of a launcher is held to
/// the same reading of the caches. The targets and the manifest of an
/// exposures deployment are two reads of its documents: the targets come
/// from the documents alone, and the manifest is derived from the documents
/// and their contracts.
#[derive(Debug)]
struct PlanSources<'a> {
    dirs: &'a PeppyDirs,
    node_cache: OnceCell<std::result::Result<Vec<NodeCacheEntry>, String>>,
    manifests: ReadSources<SourceManifest, String>,
    targets: ReadSources<DeploymentTargets, UnreadTargets>,
}

impl<'a> PlanSources<'a> {
    fn new(dirs: &'a PeppyDirs) -> Self {
        Self {
            dirs,
            node_cache: OnceCell::new(),
            manifests: RefCell::default(),
            targets: RefCell::default(),
        }
    }

    /// This machine's nodes cache, or why it is not readable.
    fn node_cache(&self) -> &std::result::Result<Vec<NodeCacheEntry>, String> {
        self.node_cache
            .get_or_init(|| core_node::load_node_cache(self.dirs).map_err(|e| e.to_string()))
    }

    /// The manifest of the deployments of `source`, or the report entry that
    /// names the source and says why its manifest is not readable on this
    /// machine.
    fn manifest_of(&self, source: &DeploymentSource) -> SourceRead<SourceManifest, String> {
        read_once(&self.manifests, source, || self.read_manifest(source))
    }

    /// The targets of the deployments of `source`: those of its exposure
    /// documents, or the slots and daemon targets of its node manifest.
    fn targets_of(
        &self,
        source: &DeploymentSource,
    ) -> SourceRead<DeploymentTargets, UnreadTargets> {
        read_once(&self.targets, source, || self.read_targets(source))
    }

    fn read_targets(
        &self,
        source: &DeploymentSource,
    ) -> std::result::Result<DeploymentTargets, UnreadTargets> {
        let DeploymentSource::Exposures { exposures } = source else {
            let manifest = self.manifest_of(source).map_err(UnreadTargets::NotCached)?;
            return DeploymentTargets::of_node(&manifest.config.manifest)
                .map_err(|refusals| UnreadTargets::Refused(refusals.join("; ")));
        };
        let documents = core_node::resolve_exposure_documents(
            self.dirs,
            exposures,
            &crate::commands::report_as_info,
        )
        .map_err(|error| match error {
            ExposureDocumentsError::NotCached(reason) => {
                UnreadTargets::NotCached(format!("{} ({reason})", source.label()))
            }
            ExposureDocumentsError::Refused(reason) => UnreadTargets::Refused(reason),
        })?;
        // A document set the launch would refuse (two sources under one target
        // name, an interface this peppy does not serve) is refused here as it
        // would be there.
        DeploymentTargets::of_exposures(&documents)
            .map_err(|e| UnreadTargets::Refused(e.to_string()))
    }

    fn read_manifest(
        &self,
        source: &DeploymentSource,
    ) -> std::result::Result<SourceManifest, String> {
        let (name, tag) = match source {
            DeploymentSource::Node { name, tag } => (name.as_str(), tag.as_str()),
            DeploymentSource::Exposures { exposures } => {
                // The built-in server's manifest is derived from the exposures
                // and their contracts, through the same caches and the same
                // derivation a launch uses.
                let plan = core_node::resolve_exposure_plan(
                    self.dirs,
                    exposures,
                    &crate::commands::report_as_info,
                )
                .map_err(|e| format!("{} ({e})", source.label()))?;
                return Ok(SourceManifest {
                    name: plan.name.as_str().to_owned(),
                    tag: plan.tag,
                    config: plan.config,
                    addressing: plan.addressing,
                });
            }
        };
        let entries = self
            .node_cache()
            .as_ref()
            .map_err(|e| format!("{} (the nodes cache is not readable: {e})", source.label()))?;
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
                let Some(checkout) = core_node::materialized_checkout(self.dirs, repo_url, commit)
                else {
                    return Err(format!(
                        "{id} (no checkout of {commit} on this machine; run `peppy repo refresh`)"
                    ));
                };
                checkout.join(path.as_path())
            }
            EntryOrigin::Oci { url, .. } => {
                // `repo refresh` stores the manifest of every node it records
                // from an oci repository; this command never reaches the
                // network, so a manifest the store lacks is named instead.
                core_node::stored_node_manifest(self.dirs, &entry.sha256).ok_or_else(|| {
                    format!(
                        "{id} (the manifest of its artifact in {url} is not on this machine; run \
                         `peppy repo refresh`)"
                    )
                })?
            }
        };
        let config =
            NodeConfigParser::from_path(&manifest_path).map_err(|e| format!("{id} ({e})"))?;
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
        Ok(SourceManifest {
            name: name.to_string(),
            tag: tag.to_string(),
            config,
            addressing: MemberAddressing::WholeSet,
        })
    }
}

/// Why the targets of one deployment were not read.
#[derive(Debug, Clone)]
enum UnreadTargets {
    /// This machine's caches do not hold what the targets are read from:
    /// the checks of the deployment are skipped, and the report names it.
    NotCached(String),
    /// What the targets are read from is one that the launch would refuse:
    /// the command fails with that refusal.
    Refused(String),
}

/// Hold the flat plan to the launch checks of daemon scopes: every scope
/// parsed into its interface's type and held against the launcher and the
/// stack `selection` resolves (for `robot_membership`, the trial join of every
/// feature set of every option), every instance with a daemon target on the
/// coordinator, no link to a daemon target, and at most one daemon target
/// of each interface on an instance. They read the exposure documents and
/// the node manifests alone, so the deployments of exposures run them with
/// a cold nodes cache too. A deployment whose documents or manifest this
/// machine's caches do not hold skips its checks, and the report names it,
/// so a clean exit is never mistaken for checked scopes; the checks of every
/// other deployment still run. A document or a manifest the launch would
/// refuse fails the command with that refusal.
///
/// Returns the instances with launch variables that a join through a scope
/// of the checked deployments can start.
fn check_daemon_scope_rules(
    prepared: &PreparedLauncher,
    flat: &PeppyLauncher,
    selection: &UnitSelection,
    copies: &CopyMembership,
    sources: &PlanSources<'_>,
    report: &mut Vec<String>,
) -> Result<VariableNeeds> {
    let mut checked: Vec<(&Deployment, Rc<DeploymentTargets>)> =
        Vec::with_capacity(flat.deployments.len());
    let mut not_cached_exposures: Vec<String> = Vec::new();
    let mut not_cached_nodes: Vec<String> = Vec::new();
    for deployment in &flat.deployments {
        match sources.targets_of(&deployment.source) {
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
    let joinable = check_daemon_scopes(
        prepared,
        checked
            .iter()
            .map(|(deployment, targets)| (*deployment, targets.as_ref())),
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
    Ok(joinable.variable_needs)
}

/// Report the launch variables of the plan, as a launch checks them. First,
/// one line for each variable of a list that the launch needs from the shell
/// that launches: each instance that takes it and what can start that
/// instance, the launch itself or a join through a scope of the launch
/// (`scoped`). Then one line for each `one_of` of which the launch needs one
/// complete set: its sets, in order, and each instance that takes a set of
/// it and what can start that instance. A script reads the sets of the line
/// between `needs one set of ` and `, from the shell that launches: `, each
/// set after `, or of `, each name in backticks.
/// Then one line for each instance with launch variables that a core node
/// link or a word places off the coordinator, which the launch refuses: this
/// command reads no daemon and no `--place` wiring, so the line names the
/// link or the machine as the file or the word writes it, and the condition
/// under which the launch runs the instance on the coordinator.
fn report_launch_variables(
    composed: &ComposedLaunch,
    joins: &[CopyReference],
    scoped: VariableNeeds,
    report: &mut Vec<String>,
) {
    let instances = || {
        composed
            .launcher
            .deployments
            .iter()
            .flat_map(|deployment| &deployment.instances)
    };
    let mut needs = VariableNeeds::default();
    for instance in instances() {
        needs.add_instance(instance.instance_id.as_str(), instance, &Starter::Launch);
    }
    needs.merge(scoped);
    for (variable, reasons) in needs.reasons() {
        report.push(format!(
            "launch variables: the launch needs `{variable}` from the shell that launches: \
             {reasons}"
        ));
    }
    for (variables, reasons) in needs.choice_reasons() {
        report.push(format!(
            "launch variables: the launch needs {}, from the shell that launches: {reasons}",
            variables.describe()
        ));
    }
    for instance in instances().filter(|instance| !instance.env_from_launch.is_empty()) {
        let Some(placement) = placement_off_the_coordinator(composed, joins, instance) else {
            continue;
        };
        report.push(format!(
            "launch variables: `{}` takes {} from the launch and {placement}; an instance with \
             launch variables runs on the coordinator only",
            instance.instance_id,
            instance.env_from_launch.describe(),
        ));
    }
}

/// Where the launch can place `instance` off the coordinator, as a clause of
/// the report, or `None` when it runs on the coordinator. An instance with no
/// `core_node` runs there. An instance of a copy runs on the machine of its
/// part's word, else on the machine of the copy's `--join` word, else on the
/// coordinator, unless `--place` names the copy, which this command does not
/// read. An instance on any other core node link runs where `--place` wires
/// that link.
fn placement_off_the_coordinator(
    composed: &ComposedLaunch,
    joins: &[CopyReference],
    instance: &DeploymentInstance,
) -> Option<String> {
    let link = instance.core_node.as_deref()?;
    let Some(copy) = composed
        .copies()
        .iter()
        .find(|copy| copy.name.as_str() == link)
    else {
        return Some(format!(
            "runs on core node link `{link}`, so the launch refuses it unless `--place \
             {link}@self` or `--local` puts that link on the coordinator"
        ));
    };
    let own_axis = copy
        .instances
        .iter()
        .find(|owned| owned.instance_id == instance.instance_id)
        .and_then(|owned| owned.own_axis.as_ref());
    let part_machine = own_axis.and_then(|axis| composed.part_machines_of(&copy.name).get(axis));
    let copy_machine = joins
        .iter()
        .find(|join| join.name == copy.name)
        .map(|join| &join.core_node);
    match part_machine.or(copy_machine) {
        Some(CoreNodeTarget::Named(machine)) => Some(format!(
            "a word places it on `{machine}`, so the launch refuses it unless `{machine}` is the \
             coordinator"
        )),
        Some(CoreNodeTarget::Coordinator) | None => None,
    }
}

/// The manifest of a deployment source as the checks read it: the identity
/// it declares, and how its node reads the sets its slots hold.
#[derive(Debug)]
struct SourceManifest {
    name: String,
    tag: String,
    config: NodeConfig,
    addressing: MemberAddressing,
}

/// One deployment's manifest as [`check_link_plan`] judges it.
struct CheckedManifest {
    /// The deployment's position in the flat launcher, which carries the
    /// instances the manifest is judged against.
    index: usize,
    manifest: Rc<SourceManifest>,
}

/// Hold the flat plan to the launch-time link rules a client can check: the
/// cross-family slot-key and vacancy rules, then the pairing rules, after
/// [`report_clocks_and_restarts`] has reported every instance's clock and
/// policy. Both validators read only the flat launcher and the deployed nodes'
/// manifests, so they run here without a daemon; the contract-binding rules
/// stay launch-only because satisfying them can involve the root node, which
/// only the daemon knows.
///
/// Manifests come from `sources`: the nodes cache, a git-backed entry's from
/// the checkout the caches materialized for it. The check runs only when every
/// deployed node's manifest is readable on this machine: the pairing rules
/// judge links by both endpoints' declarations, so validating a partial item
/// list would trade missed errors for false ones. When something is missing
/// the report says which check was skipped and why, so a clean exit is never
/// mistaken for a validated plan.
fn check_link_plan(
    flat: &PeppyLauncher,
    copies: &CopyMembership,
    sources: &PlanSources<'_>,
    report: &mut Vec<String>,
) -> Result<()> {
    match sources.node_cache() {
        Ok(entries) if !entries.is_empty() => {}
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
    }

    let mut unavailable: Vec<String> = Vec::new();
    let mut manifests: Vec<CheckedManifest> = Vec::new();
    for (index, deployment) in flat.deployments.iter().enumerate() {
        match sources.manifest_of(&deployment.source) {
            Ok(manifest) => manifests.push(CheckedManifest { index, manifest }),
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
            node_name: &checked.manifest.name,
            node_tag: &checked.manifest.tag,
            instances: &flat.deployments[checked.index].instances,
            depends_on: checked.manifest.config.manifest.depends_on.as_ref(),
            implements: &checked.manifest.config.manifest.implements,
            addressing: &checked.manifest.addressing,
        })
        .collect();
    let pairing_items: Vec<PairingValidationItem<'_>> = manifests
        .iter()
        .map(|checked| PairingValidationItem {
            node_name: &checked.manifest.name,
            node_tag: &checked.manifest.tag,
            instances: &flat.deployments[checked.index].instances,
            pairing_deps: checked
                .manifest
                .config
                .manifest
                .depends_on
                .as_ref()
                .map_or_default(|d| d.pairings.as_slice()),
            observer_deps: checked
                .manifest
                .config
                .manifest
                .depends_on
                .as_ref()
                .map_or_default(|d| d.pairing_observers.as_slice()),
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
                manifest_by_index.get(&index).map(|checked| {
                    resolve_restart(request, checked.manifest.config.framework.restart)
                })
            });
            report.push(match resolved {
                Some(resolved) => format!("{id}: {clock}, restart {resolved}"),
                None => format!("{id}: {clock}, restart not resolved (manifest unavailable)"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--selections` takes the place of `--with` and `--join`: the input is
    /// one selection or the file, never both.
    #[test]
    fn the_input_is_the_selection_of_the_words_or_the_selections_file() {
        let word: LaunchWord = "sim".parse().expect("a word");
        let copy: CopyReference = "real:alpha".parse().expect("a copy");
        let file = PathBuf::from("-");

        assert_eq!(
            ResolveInput::parse(vec![word.clone()], vec![copy.clone()], None).expect("one"),
            ResolveInput::One(LaunchSelection {
                words: vec![word.clone()],
                joins: vec![copy.clone()],
            })
        );
        assert_eq!(
            ResolveInput::parse(vec![], vec![], Some(file.clone())).expect("the file"),
            ResolveInput::Many(file.clone())
        );
        for (words, joins) in [(vec![word], vec![]), (vec![], vec![copy])] {
            let error = ResolveInput::parse(words, joins, Some(file.clone()))
                .expect_err("a selection next to the file is refused")
                .to_string();
            assert!(
                error.contains("`--selections` takes the place of `--with` and `--join`"),
                "{error}"
            );
        }
    }

    /// A value whose serialization always fails.
    struct NotSerializable;

    impl Serialize for NotSerializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("no JSON form"))
        }
    }

    /// A flat launcher that does not serialize gives one whole `error` line,
    /// with the message of a lone resolve, and no part of the record.
    #[test]
    fn a_flat_launcher_that_does_not_serialize_gives_a_whole_error_line() {
        let report = ["a line of the report".to_owned()];
        let line = SelectionRecord::Resolved {
            launcher: &NotSerializable,
            report: &report,
        }
        .to_line();

        let expected = serde_json::json!({
            "error": flat_launcher_not_serialized("no JSON form").to_string(),
        });
        assert_eq!(
            String::from_utf8(line).expect("the line is UTF-8"),
            format!("{expected}\n")
        );
    }

    /// A record that serializes is one line of JSON that ends with its
    /// newline.
    #[test]
    fn a_record_is_one_line_of_json() {
        let report = ["first".to_owned(), "second".to_owned()];
        let line = SelectionRecord::Resolved {
            launcher: &serde_json::json!({ "deployments": [] }),
            report: &report,
        }
        .to_line();

        assert_eq!(
            String::from_utf8(line).expect("the line is UTF-8"),
            "{\"launcher\":{\"deployments\":[]},\"report\":[\"first\",\"second\"]}\n"
        );
    }
}
