mod action_poll;
pub mod clock;
mod colors;
mod confirm;
pub mod container;
pub mod info;
pub mod mcp;
pub mod node;
pub mod platform;
pub mod repo;
pub mod service;
pub mod stack;
mod table;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use config::node::EndpointKind;
use config::runtime::{ClockDomainId, ClockIncarnation};
use core_node_api::encoding::InstanceEndpoints;
use core_node_api::{InstanceEndpoint, InstanceState, SerializedNodeGraph};

use crate::{
    context::{AppContext, DaemonConnection},
    error::{Error, Result},
};

/// Instance ID used by the CLI when communicating with the daemon.
pub(crate) const CALLER_INSTANCE_ID: &str = "peppy-cli";

/// Timeout for action goals to be accepted by the daemon (should be fast).
pub(crate) const GOAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of lines to display in the scrolling output region.
pub(crate) const SCROLLING_OUTPUT_LINES: usize = 10;

/// How many leading hex digits of an incarnation tell two lifetimes of one
/// domain apart in a listing.
pub(crate) const INCARNATION_LABEL_DIGITS: usize = 8;

/// Single source of truth for the word `stack list` and `node info` print for
/// an instance's health, so the two commands can never drift apart on it.
pub(crate) fn health_label(healthy: bool) -> &'static str {
    if healthy { "healthy" } else { "unhealthy" }
}

/// The health cell for an instance, accounting for its lifecycle state. A
/// terminal instance (`Finished`/`Failed`) has exited, so its last health probe
/// is meaningless: render a neutral `-` rather than a stale `healthy`/
/// `unhealthy`. Live instances render their probed health via [`health_label`].
/// Shared by `stack list` and `node info` so the two never diverge.
pub(crate) fn instance_health_label(state: InstanceState, healthy: bool) -> &'static str {
    if state.is_terminal() {
        "-"
    } else {
        health_label(healthy)
    }
}

/// How one listing names the clock domains in it, so that every row is
/// distinguishable.
///
/// A domain reads as `name@core_node`, which two lifetimes of one name on one
/// machine share: a publisher that stopped leaves its consumers reading the
/// instant it froze on, and a replacement under that name is a second timeline
/// beside it. An incarnation is minted and never typed, so it earns a place in
/// output only where it is what tells two rows apart. Shared by `clock list`
/// and `stack list` so the two name a domain the same way.
pub(crate) struct DomainLabels {
    /// The `name@core_node` renderings this listing holds more than one
    /// lifetime of.
    ambiguous: BTreeSet<String>,
}

impl DomainLabels {
    /// Reads the whole listing up front, which is what lets [`Self::label`]
    /// know what else is in it.
    pub(crate) fn of<'a>(domains: impl IntoIterator<Item = &'a ClockDomainId>) -> Self {
        let mut lifetimes: BTreeMap<String, BTreeSet<ClockIncarnation>> = BTreeMap::new();
        for domain in domains {
            lifetimes
                .entry(domain.to_string())
                .or_default()
                .insert(domain.incarnation);
        }
        Self {
            ambiguous: lifetimes
                .into_iter()
                .filter(|(_, incarnations)| incarnations.len() > 1)
                .map(|(rendered, _)| rendered)
                .collect(),
        }
    }

    /// `name@core_node`, carrying the incarnation's leading digits after a `#`
    /// where the listing holds another lifetime of that name on that machine.
    pub(crate) fn label(&self, domain: &ClockDomainId) -> String {
        let rendered = domain.to_string();
        if !self.ambiguous.contains(rendered.as_str()) {
            return rendered;
        }
        let digits = format!("{:016x}", domain.incarnation.get());
        format!("{rendered}#{}", &digits[..INCARNATION_LABEL_DIGITS])
    }
}

/// Renders the endpoints of started instances the way every command that
/// starts or inspects instances prints them: one block per declared kind,
/// `Web pages:` before `MCP endpoints:`, each printed only when it has an
/// entry. Within a block the instances come in instance id order, each as
/// `instance_id (node_label) @core_node`, their endpoints in label order
/// with the label column padded to the longest label of the block and the
/// URLs under it in the order the daemon expanded them. Empty input renders
/// nothing.
///
/// With `colorize` set, every field carries the tint its kind has in the
/// tables (instance ids magenta, node labels cyan, core nodes blue, endpoint
/// labels yellow) so one instance's lines read as a group at a glance, and
/// the headings turn bold. The URLs stay in the terminal's plain foreground:
/// they are what an operator reads off the block, so they keep the highest
/// contrast. Coloring is purely additive, the padding is measured on the
/// plain labels, so a colored block lines up exactly like its plain form.
pub fn render_endpoints(entries: &[InstanceEndpoints], colorize: bool) -> String {
    use std::fmt::Write as _;

    use colors::{
        CORE_NODE_COLOR, ENDPOINT_LABEL_COLOR, HEADING_STYLE, INSTANCE_COLOR, NODE_COLOR, paint,
    };

    let mut out = String::new();
    let mut ordered: Vec<&InstanceEndpoints> = entries.iter().collect();
    ordered.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    for (kind, heading) in [
        (EndpointKind::Page, "Web pages:"),
        (EndpointKind::Mcp, "MCP endpoints:"),
    ] {
        let block: Vec<(&InstanceEndpoints, Vec<&InstanceEndpoint>)> = ordered
            .iter()
            .map(|entry| {
                let mut endpoints: Vec<&InstanceEndpoint> = entry
                    .endpoints
                    .iter()
                    .filter(|endpoint| endpoint.kind == kind)
                    .collect();
                endpoints.sort_by(|a, b| a.label.cmp(&b.label));
                (*entry, endpoints)
            })
            .filter(|(_, endpoints)| !endpoints.is_empty())
            .collect();
        if block.is_empty() {
            continue;
        }
        let label_width = block
            .iter()
            .flat_map(|(_, endpoints)| endpoints.iter().map(|endpoint| endpoint.label.len()))
            .max()
            .unwrap_or(0);
        let _ = writeln!(&mut out, "{}", paint(colorize, HEADING_STYLE, heading));
        for (entry, endpoints) in block {
            let _ = writeln!(
                &mut out,
                "  {} ({}) @{}",
                paint(colorize, INSTANCE_COLOR, &entry.instance_id),
                paint(colorize, NODE_COLOR, &entry.node_label),
                paint(colorize, CORE_NODE_COLOR, &entry.core_node)
            );
            for endpoint in endpoints {
                // The label heads its first URL; the rest sit under it in a
                // blank column. An endpoint the daemon expanded to no URL at
                // all contributes no line.
                for (index, url) in endpoint.urls.iter().enumerate() {
                    let label = if index == 0 {
                        endpoint.label.as_str()
                    } else {
                        ""
                    };
                    // The column is padded on the plain label and the tint
                    // applied to it alone: padding a painted label would count
                    // its zero-width escapes as columns and pull the URLs out
                    // of line.
                    let padding = " ".repeat(label_width - label.len());
                    let _ = writeln!(
                        &mut out,
                        "    {}{padding}  {url}",
                        paint(colorize, ENDPOINT_LABEL_COLOR, label)
                    );
                }
            }
        }
    }
    out
}

/// The blocks [`render_endpoints`] produces, on the node log, for the
/// commands that start instances. The one place the rendered block reaches
/// an operator, so a change to how it is delivered is made once. Colored
/// under the CLI's shared color gate, the same one the log formatter and the
/// tables read, so a `NO_COLOR` or non-interactive run prints the plain
/// block. That one gate is also what tells the log formatter to write a
/// message's control characters through rather than escape them, so the
/// tints painted here are the tints an operator sees.
pub fn log_endpoints(entries: &[InstanceEndpoints]) {
    for line in render_endpoints(entries, crate::terminal::colors_enabled()).lines() {
        tracing::info!("{line}");
    }
}

/// Trait for executable commands
pub trait Command {
    /// Execute the command
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()>;
}

/// Parses the daemon's serialized stack graph from its JSON payload. Single
/// owner of the parse and its error message so the commands that read the stack
/// snapshot (stack list, node run, node remove, node runtime-config) cannot
/// drift on it.
pub(crate) fn parse_stack_graph(graph_json: &str) -> Result<SerializedNodeGraph> {
    serde_json::from_str(graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))
}

/// Shared core of the remote-target gates below: refuses a `--core-node`
/// override naming anything but the local daemon. `reason` explains, in terms
/// of the failing command, why its request cannot cross machines; it is built
/// lazily so the happy path allocates nothing.
fn reject_remote_target(
    conn: &DaemonConnection<'_>,
    command: &str,
    reason: impl FnOnce(&str) -> String,
) -> Result<()> {
    if conn.target_core_node != conn.core_node_name {
        return Err(Error::ExecutionFailed(format!(
            "`{command}` does not support --core-node: {} \
             Run the command on that daemon's machine instead.",
            reason(&conn.target_core_node)
        )));
    }
    Ok(())
}

/// Guards the commands that route every request to the LOCAL daemon regardless
/// of `--core-node`.
///
/// `node run` is the shape: its preflight lookup, its build, and its start all
/// address `conn.core_node_name`, so accepting an override would report success
/// against the machine the operator did not name. The goal itself is now
/// host-independent (the daemon assembles the runtime config from its own
/// state), so what remains is purely that the routing has not been taught to
/// follow the override. Refusing is the honest answer until it is.
pub(crate) fn reject_remote_target_for_local_routing(
    conn: &DaemonConnection<'_>,
    command: &str,
) -> Result<()> {
    reject_remote_target(conn, command, |target| {
        format!(
            "it addresses this machine's daemon for every step, so it would not run on \
             daemon '{target}' even though you named it."
        )
    })
}

/// Guards the commands whose request embeds a **caller-local filesystem path**
/// that the daemon resolves on its own machine (e.g. `node init`'s scaffold
/// dir, defaulting to the caller's cwd): sent to a remote daemon, the path
/// would silently be read or created on that machine's filesystem while the
/// CLI reports success with a local-looking path. Until such requests carry no
/// caller-local paths, these commands refuse a `--core-node` override naming
/// anything but the local daemon. Sibling of
/// [`reject_remote_target_for_local_routing`].
pub(crate) fn reject_remote_target_for_local_path(
    conn: &DaemonConnection<'_>,
    command: &str,
) -> Result<()> {
    reject_remote_target(conn, command, |target| {
        format!(
            "it operates on a filesystem path from this machine, which would instead \
             be resolved on daemon '{target}''s filesystem."
        )
    })
}

pub(crate) fn block_on<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(future)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_label_is_the_single_source_of_truth() {
        // `stack list` and `node info` both render this; pin it so they cannot
        // drift apart.
        assert_eq!(health_label(true), "healthy");
        assert_eq!(health_label(false), "unhealthy");
    }

    #[test]
    fn instance_health_label_neutralizes_terminal_states() {
        // Live instances report their probed health.
        assert_eq!(
            instance_health_label(InstanceState::Running, true),
            "healthy"
        );
        assert_eq!(
            instance_health_label(InstanceState::Starting, false),
            "unhealthy"
        );
        // Terminal instances have exited, so health is not applicable; the
        // stale `healthy` flag must never surface as a verdict.
        assert_eq!(instance_health_label(InstanceState::Finished, true), "-");
        assert_eq!(instance_health_label(InstanceState::Finished, false), "-");
        assert_eq!(instance_health_label(InstanceState::Failed, true), "-");
        assert_eq!(instance_health_label(InstanceState::Failed, false), "-");
    }

    fn endpoint(label: &str, kind: EndpointKind, urls: &[&str]) -> InstanceEndpoint {
        InstanceEndpoint {
            label: label.to_string(),
            kind,
            urls: urls.iter().map(|url| url.to_string()).collect(),
        }
    }

    fn instance(
        instance_id: &str,
        node_label: &str,
        endpoints: Vec<InstanceEndpoint>,
    ) -> InstanceEndpoints {
        InstanceEndpoints {
            instance_id: instance_id.to_string(),
            node_label: node_label.to_string(),
            core_node: "cn-sweet-edison".to_string(),
            endpoints,
        }
    }

    /// Pages and MCP endpoints land under their own headings, pages first,
    /// with per-block label padding and every URL under its label.
    #[test]
    fn render_endpoints_groups_kinds_under_their_headings_in_order() {
        let out = render_endpoints(
            &[
                instance(
                    "alpha_commander_inst",
                    "mcp_openarm_v2_v1_scene_lighting_v1:builtin",
                    vec![
                        endpoint(
                            "scene_lighting_v1",
                            EndpointKind::Mcp,
                            &["http://127.0.0.1:8900/scene_lighting/v1/mcp"],
                        ),
                        endpoint(
                            "openarm_v2_v1",
                            EndpointKind::Mcp,
                            &["http://127.0.0.1:8900/openarm_v2/v1/mcp"],
                        ),
                    ],
                ),
                instance(
                    "simulation_inst",
                    "waldo:v1",
                    vec![endpoint(
                        "viewer",
                        EndpointKind::Page,
                        &["https://127.0.0.1:8080/", "https://100.123.58.116:8080/"],
                    )],
                ),
            ],
            false,
        );
        assert_eq!(
            out,
            "\
Web pages:
  simulation_inst (waldo:v1) @cn-sweet-edison
    viewer  https://127.0.0.1:8080/
            https://100.123.58.116:8080/
MCP endpoints:
  alpha_commander_inst (mcp_openarm_v2_v1_scene_lighting_v1:builtin) @cn-sweet-edison
    openarm_v2_v1      http://127.0.0.1:8900/openarm_v2/v1/mcp
    scene_lighting_v1  http://127.0.0.1:8900/scene_lighting/v1/mcp
"
        );
    }

    /// An input with one kind prints one heading, and instances of that kind
    /// come in instance id order whatever order they arrived in.
    #[test]
    fn render_endpoints_prints_one_heading_for_one_kind_and_orders_instances() {
        let out = render_endpoints(
            &[
                instance(
                    "simulation_inst",
                    "waldo:v1",
                    vec![endpoint(
                        "viewer",
                        EndpointKind::Page,
                        &["https://127.0.0.1:8080/"],
                    )],
                ),
                instance(
                    "alpha_commander_inst",
                    "openarm_web_commander:v1",
                    vec![endpoint(
                        "panel",
                        EndpointKind::Page,
                        &["http://127.0.0.1:8765", "http://100.123.58.116:8765"],
                    )],
                ),
            ],
            false,
        );
        assert_eq!(
            out,
            "\
Web pages:
  alpha_commander_inst (openarm_web_commander:v1) @cn-sweet-edison
    panel   http://127.0.0.1:8765
            http://100.123.58.116:8765
  simulation_inst (waldo:v1) @cn-sweet-edison
    viewer  https://127.0.0.1:8080/
"
        );
        assert!(!out.contains("MCP endpoints:"));
    }

    /// Padding is per block: a long MCP label does not widen the page block,
    /// and an instance serving both kinds appears in both blocks.
    #[test]
    fn render_endpoints_pads_labels_per_block() {
        let out = render_endpoints(
            &[instance(
                "hybrid_inst",
                "hybrid:v1",
                vec![
                    endpoint("ui", EndpointKind::Page, &["http://127.0.0.1:8000"]),
                    endpoint(
                        "a_very_long_exposure_label_v1",
                        EndpointKind::Mcp,
                        &["http://127.0.0.1:8900/a/v1/mcp"],
                    ),
                ],
            )],
            false,
        );
        assert_eq!(
            out,
            "\
Web pages:
  hybrid_inst (hybrid:v1) @cn-sweet-edison
    ui  http://127.0.0.1:8000
MCP endpoints:
  hybrid_inst (hybrid:v1) @cn-sweet-edison
    a_very_long_exposure_label_v1  http://127.0.0.1:8900/a/v1/mcp
"
        );
    }

    /// Colorizing tints each field with its kind's color and nothing else:
    /// stripping the codes back out has to reproduce the plain block byte for
    /// byte, which is what keeps the URL column aligned under a padded label.
    #[test]
    fn render_endpoints_colorize_is_purely_additive() {
        use crate::commands::colors::{
            CORE_NODE_COLOR, ENDPOINT_LABEL_COLOR, HEADING_STYLE, INSTANCE_COLOR, NODE_COLOR, RESET,
        };
        use crate::commands::table::strip_ansi;

        let entries = [instance(
            "simulation_inst",
            "waldo:v1",
            vec![
                endpoint(
                    "viewer",
                    EndpointKind::Page,
                    &["https://127.0.0.1:8080/", "https://100.123.58.116:8080/"],
                ),
                endpoint(
                    "a_much_longer_label",
                    EndpointKind::Page,
                    &["https://127.0.0.1:8081/"],
                ),
            ],
        )];
        let plain = render_endpoints(&entries, false);
        let colored = render_endpoints(&entries, true);

        assert!(
            !plain.contains('\x1b'),
            "plain output must stay free of ANSI codes:\n{plain:?}"
        );
        assert_eq!(
            strip_ansi(&colored),
            plain,
            "stripping colors must reproduce the plain block exactly"
        );
        for (code, field) in [
            (HEADING_STYLE, "Web pages:"),
            (INSTANCE_COLOR, "simulation_inst"),
            (NODE_COLOR, "waldo:v1"),
            (CORE_NODE_COLOR, "cn-sweet-edison"),
            (ENDPOINT_LABEL_COLOR, "viewer"),
        ] {
            assert!(
                colored.contains(&format!("{code}{field}{RESET}")),
                "{field} should carry its own color:\n{colored:?}"
            );
        }
        // The URLs are the block's payload and stay in the plain foreground,
        // and a continuation line's empty label column carries no codes.
        let continuation = colored
            .lines()
            .find(|line| line.contains("100.123.58.116"))
            .expect("the second URL sits on a continuation line");
        assert!(
            !continuation.contains('\x1b'),
            "a continuation URL and its blank label column stay plain:\n{continuation:?}"
        );
        assert_eq!(
            continuation.trim_start(),
            "https://100.123.58.116:8080/",
            "the continuation line carries the URL alone"
        );
    }

    #[test]
    fn render_endpoints_renders_nothing_for_empty_input() {
        assert_eq!(render_endpoints(&[], false), "");
        assert_eq!(
            render_endpoints(&[instance("silent_inst", "silent:v1", Vec::new())], false),
            ""
        );
    }

    #[test]
    fn block_on_runs_a_future_without_an_ambient_runtime() {
        // The no-current-handle branch builds a fresh runtime.
        let value = block_on(async { Ok::<_, crate::error::Error>(7) }).expect("future resolves");
        assert_eq!(value, 7);
    }

    #[test]
    fn remote_target_gate_rejects_only_a_differing_target() {
        use peppylib::MessengerHandle;
        use pmi::MessengerBackend as _;
        block_on(async {
            let mut instance = pmi::MockAdapter::start_router()
                .await
                .expect("mock router should start");
            instance
                .messenger()
                .start_session()
                .await
                .expect("mock session should start");
            let handle = MessengerHandle::from_shared(std::sync::Arc::new(
                tokio::sync::Mutex::new(instance.take_messenger()),
            ));
            let conn = |target: &str| DaemonConnection {
                messenger: &handle,
                core_node_name: "local-daemon".to_string(),
                target_core_node: target.to_string(),
                target_is_override: target != "local-daemon",
                git_hash: "test-git-hash".to_string(),
                shutdown_grace_secs: 5,
            };

            // Target == local (the no-override shape): allowed.
            reject_remote_target_for_local_routing(&conn("local-daemon"), "peppy node run")
                .expect("a local target must pass the gate");

            // A differing target is refused with an actionable message.
            let err = reject_remote_target_for_local_routing(&conn("robot-7"), "peppy node run")
                .expect_err("a remote target must be refused");
            let msg = err.to_string();
            assert!(msg.contains("--core-node"), "names the flag: {msg}");
            assert!(msg.contains("peppy node run"), "names the command: {msg}");
            assert!(msg.contains("robot-7"), "names the target daemon: {msg}");

            // The local-path sibling gate behaves identically: local target
            // passes, a remote target is refused naming flag/command/target.
            reject_remote_target_for_local_path(&conn("local-daemon"), "peppy node init")
                .expect("a local target must pass the path gate");
            let err = reject_remote_target_for_local_path(&conn("robot-7"), "peppy node init")
                .expect_err("a remote target must be refused by the path gate");
            let msg = err.to_string();
            assert!(msg.contains("--core-node"), "names the flag: {msg}");
            assert!(msg.contains("peppy node init"), "names the command: {msg}");
            assert!(msg.contains("robot-7"), "names the target daemon: {msg}");
            Ok(())
        })
        .expect("gate test future resolves");
    }
}
