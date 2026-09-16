//! Every clock domain the federation is running, gathered from the daemons
//! that host them.
//!
//! A domain is hosted by one daemon and read wherever its consumers run, so no
//! single daemon knows the whole picture. This asks every live one and puts
//! the answers together, which is what makes a domain and the instances
//! reading it legible as one thing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{ClockConsumerInfo, ClockDomainInfo, ClockListRequest};
use futures::future::join_all;
use peppylib::{CoreNodePresenceMessenger, core_node::transport::poll};

use crate::commands::DomainLabels;
use crate::commands::colors::{BINDING_COLOR, COUNT_COLOR, INSTANCE_COLOR, NODE_COLOR, paint};
use crate::commands::table::render_table;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const HEADERS: [&str; 6] = [
    "CLOCK",
    "SOURCE",
    "PUBLISHER",
    "OWNER",
    "READY",
    "CONSUMERS",
];

/// What every live daemon reported, and the ones that did not answer.
struct Gathered {
    domains: Vec<ClockDomainInfo>,
    /// Consumers from every daemon, each with the machine it runs on.
    consumers: Vec<(String, ClockConsumerInfo)>,
    failed: Vec<String>,
}

impl Gathered {
    /// Orders the listing by domain identity, which is total, so two
    /// lifetimes of one name hold a stable place beside each other.
    fn sorted(mut self) -> Self {
        self.domains.sort_by(|a, b| a.domain.cmp(&b.domain));
        self
    }
}

pub fn list_clocks(ctx: &Arc<AppContext>, json: bool) -> Result<()> {
    let gathered = crate::commands::block_on(gather(ctx))?;
    if json {
        println!("{}", render_json(&gathered));
    } else {
        print!(
            "{}",
            render_table_output(
                &gathered,
                crate::terminal::colors_enabled(),
                crate::terminal::stdout_width(),
            )
        );
    }
    if gathered.failed.is_empty() {
        return Ok(());
    }
    Err(Error::ExecutionFailed(format!(
        "clock list failed for: {}",
        gathered.failed.join(", ")
    )))
}

async fn gather(ctx: &Arc<AppContext>) -> Result<Gathered> {
    let conn = ctx.connect_to_daemon().await?;
    let live = CoreNodePresenceMessenger::list_live(
        conn.messenger,
        conn.target_is_override
            .then_some(conn.target_core_node.as_str()),
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await?;
    let targets: Vec<String> = live.into_iter().map(|claim| claim.core_node).collect();

    let answers = join_all(targets.into_iter().map(|core_node| {
        let messenger = conn.messenger;
        let caller = &conn.core_node_name;
        async move {
            let answer = poll(
                &ClockListRequest::new(),
                messenger,
                caller,
                crate::commands::CALLER_INSTANCE_ID,
                &core_node,
                REQUEST_TIMEOUT,
            )
            .await;
            (core_node, answer)
        }
    }))
    .await;

    let mut gathered = Gathered {
        domains: Vec::new(),
        consumers: Vec::new(),
        failed: Vec::new(),
    };
    for (core_node, answer) in answers {
        match answer {
            Ok(response) => {
                gathered.domains.extend(response.domains);
                gathered.consumers.extend(
                    response
                        .consumers
                        .into_iter()
                        .map(|consumer| (core_node.clone(), consumer)),
                );
            }
            Err(_) => gathered.failed.push(core_node),
        }
    }
    Ok(gathered.sorted())
}

/// How many instances, anywhere, read this domain.
fn consumer_count(gathered: &Gathered, info: &ClockDomainInfo) -> usize {
    gathered
        .consumers
        .iter()
        .filter(|(_, consumer)| consumer.domain == info.domain)
        .count()
}

/// The word a domain's state reads as: `waiting` until the domain's first
/// tick reaches the daemon hosting it, and `ready` from then on.
fn readiness(info: &ClockDomainInfo) -> &'static str {
    if info.ready { "ready" } else { "waiting" }
}

/// Who to ask about a domain: the launch the hosting daemon reports for it,
/// or `peppy node run` when it reports none.
fn owner(info: &ClockDomainInfo) -> String {
    info.launch.as_ref().map_or_else(
        || "peppy node run".to_owned(),
        |launch| {
            format!(
                "launch {} ({})",
                launch.launch_id, launch.coordinator_core_node
            )
        },
    )
}

fn render_table_output(gathered: &Gathered, colorize: bool, max_width: Option<usize>) -> String {
    let labels = DomainLabels::of(gathered.domains.iter().map(|info| &info.domain));
    let mut rows: Vec<Vec<String>> = vec![vec![
        paint(colorize, NODE_COLOR, "wall"),
        "built-in".to_owned(),
        "-".to_owned(),
        "-".to_owned(),
        "ready".to_owned(),
        "-".to_owned(),
    ]];
    for info in &gathered.domains {
        rows.push(vec![
            paint(colorize, NODE_COLOR, &labels.label(&info.domain)),
            "simulated".to_owned(),
            paint(colorize, INSTANCE_COLOR, &info.publisher_instance_id),
            paint(colorize, BINDING_COLOR, &owner(info)),
            readiness(info).to_owned(),
            paint(
                colorize,
                COUNT_COLOR,
                &consumer_count(gathered, info).to_string(),
            ),
        ]);
    }
    let mut out = String::new();
    render_table(&mut out, &HEADERS, &[rows], max_width);
    out
}

/// A domain's identity as the JSON carries it, for an entry to extend.
fn json_identity(
    domain: &config::runtime::ClockDomainId,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("clock".to_owned(), domain.to_string().into()),
        ("name".to_owned(), domain.name.as_str().into()),
        ("core_node".to_owned(), domain.core_node.as_str().into()),
        ("incarnation".to_owned(), domain.incarnation.get().into()),
    ])
}

fn render_json(gathered: &Gathered) -> String {
    // Keyed by the whole domain identity: two lifetimes of one name read as
    // `name@core_node` alike, and each one's consumers are its own.
    let mut by_domain: BTreeMap<&config::runtime::ClockDomainId, Vec<String>> = BTreeMap::new();
    for (core_node, consumer) in &gathered.consumers {
        by_domain
            .entry(&consumer.domain)
            .or_default()
            .push(format!("{}@{core_node}", consumer.instance_id));
    }
    let listed: BTreeSet<&config::runtime::ClockDomainId> =
        gathered.domains.iter().map(|info| &info.domain).collect();

    let domains: Vec<serde_json::Value> = gathered
        .domains
        .iter()
        .map(|info| {
            let mut entry = json_identity(&info.domain);
            entry.extend([
                (
                    "publisher".to_owned(),
                    info.publisher_instance_id.as_str().into(),
                ),
                ("owner".to_owned(), owner(info).into()),
                ("ready".to_owned(), info.ready.into()),
                (
                    "last_tick_ns".to_owned(),
                    serde_json::json!(info.last_tick_ns),
                ),
                (
                    "consumers".to_owned(),
                    serde_json::json!(by_domain.get(&info.domain).cloned().unwrap_or_default()),
                ),
            ]);
            entry.into()
        })
        .collect();

    // A domain some instance reads that no answering daemon reported hosting.
    // Its readers stay visible when the machine hosting it is one of the ones
    // that did not answer.
    let unreported: Vec<serde_json::Value> = by_domain
        .into_iter()
        .filter(|(domain, _)| !listed.contains(domain))
        .map(|(domain, consumers)| {
            let mut entry = json_identity(domain);
            entry.insert("consumers".to_owned(), serde_json::json!(consumers));
            entry.into()
        })
        .collect();

    serde_json::json!({
        "wall": { "clock": "wall", "source": "built-in" },
        "domains": domains,
        "unreported_domains": unreported,
        "unreachable": gathered.failed,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::LaunchIdentity;

    /// Two lifetimes of one name on one machine, which is what a publisher
    /// that stopped and a replacement under its name leave behind.
    const FROZEN: u64 = 0x1111_2222_3333_4444;
    const FRESH: u64 = 0xaaaa_bbbb_cccc_dddd;

    fn domain(name: &str, core_node: &str, incarnation: u64) -> config::runtime::ClockDomainId {
        config::runtime::ClockDomainId::new(
            config::runtime::Name::new(name).expect("a domain name"),
            config::runtime::CoreNodeName::new(core_node).expect("a machine name"),
            config::runtime::ClockIncarnation::try_from(incarnation).expect("non-zero"),
        )
    }

    fn hosted(domain: config::runtime::ClockDomainId, publisher: &str) -> ClockDomainInfo {
        ClockDomainInfo {
            domain,
            publisher_instance_id: publisher.to_owned(),
            launch: None,
            ready: true,
            last_tick_ns: Some(42),
        }
    }

    fn reader(
        core_node: &str,
        instance_id: &str,
        reads: config::runtime::ClockDomainId,
    ) -> (String, ClockConsumerInfo) {
        (
            core_node.to_owned(),
            ClockConsumerInfo {
                instance_id: instance_id.to_owned(),
                domain: reads,
            },
        )
    }

    fn gathered(
        domains: Vec<ClockDomainInfo>,
        consumers: Vec<(String, ClockConsumerInfo)>,
        failed: Vec<String>,
    ) -> Gathered {
        Gathered {
            domains,
            consumers,
            failed,
        }
    }

    fn parsed(listing: &Gathered) -> serde_json::Value {
        serde_json::from_str(&render_json(listing)).expect("the listing is valid JSON")
    }

    fn strings(value: &serde_json::Value) -> Vec<String> {
        value
            .as_array()
            .expect("an array")
            .iter()
            .map(|item| item.as_str().expect("a string").to_owned())
            .collect()
    }

    /// The listing orders by the identity itself, which is total: name, then
    /// machine, then lifetime. Two lifetimes of one name on one machine each
    /// hold a stable place.
    #[test]
    fn domains_sort_by_identity() {
        let sorted = gathered(
            vec![
                hosted(domain("robot", "cn-b", 2), "sim_b"),
                hosted(domain("robot", "cn-a", 9), "sim_a2"),
                hosted(domain("robot", "cn-a", 4), "sim_a1"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .sorted();
        let order: Vec<u64> = sorted
            .domains
            .iter()
            .map(|info| info.domain.incarnation.get())
            .collect();
        assert_eq!(order, vec![4, 9, 2], "name, then machine, then lifetime");
    }

    /// Two lifetimes of one name are two timelines, so the readers of each
    /// one are reported under the identity they actually read.
    #[test]
    fn consumers_group_by_the_whole_domain_identity() {
        let listing = gathered(
            vec![
                hosted(domain("robot", "cn-a", FROZEN), "sim_1"),
                hosted(domain("robot", "cn-a", FRESH), "sim_2"),
            ],
            vec![
                reader("cn-a", "old_reader", domain("robot", "cn-a", FROZEN)),
                reader("cn-b", "new_reader", domain("robot", "cn-a", FRESH)),
            ],
            Vec::new(),
        );
        let json = parsed(&listing);
        let consumers_of = |incarnation: u64| {
            json["domains"]
                .as_array()
                .expect("domains is an array")
                .iter()
                .find(|entry| entry["incarnation"] == incarnation)
                .map(|entry| strings(&entry["consumers"]))
                .expect("each listed domain carries its own consumers")
        };
        assert_eq!(consumers_of(FROZEN), vec!["old_reader@cn-a".to_owned()]);
        assert_eq!(consumers_of(FRESH), vec!["new_reader@cn-b".to_owned()]);
    }

    /// A reader of stdout sees which daemons the fan-out missed, so a listing
    /// gathered from part of the federation is never read as the whole of it.
    #[test]
    fn the_json_names_the_daemons_that_did_not_answer() {
        let json = parsed(&gathered(
            Vec::new(),
            Vec::new(),
            vec!["cn-b".to_owned(), "cn-c".to_owned()],
        ));
        assert_eq!(
            strings(&json["unreachable"]),
            vec!["cn-b".to_owned(), "cn-c".to_owned()]
        );
    }

    /// When the daemon hosting a domain is the one that did not answer, the
    /// instances reading it are on daemons that did, so they are reported
    /// under the domain they name.
    #[test]
    fn consumers_of_an_unreported_domain_are_still_listed() {
        let json = parsed(&gathered(
            Vec::new(),
            vec![reader("cn-a", "arm_1", domain("robot", "cn-b", 7))],
            vec!["cn-b".to_owned()],
        ));
        let unreported = json["unreported_domains"]
            .as_array()
            .expect("unreported_domains is an array");
        assert_eq!(unreported.len(), 1, "one unreported domain: {unreported:?}");
        assert_eq!(unreported[0]["clock"], "robot@cn-b");
        assert_eq!(unreported[0]["incarnation"], 7);
        assert_eq!(
            strings(&unreported[0]["consumers"]),
            vec!["arm_1@cn-a".to_owned()]
        );
    }

    /// Two lifetimes of one name on one machine render alike, so the table
    /// carries the incarnation that tells them apart. A single lifetime keeps
    /// reading as `name@core_node`.
    #[test]
    fn the_table_tells_two_lifetimes_of_one_name_apart() {
        let both = render_table_output(
            &gathered(
                vec![
                    hosted(domain("robot", "cn-a", FROZEN), "sim_1"),
                    hosted(domain("robot", "cn-a", FRESH), "sim_2"),
                ],
                Vec::new(),
                Vec::new(),
            ),
            false,
            None,
        );
        assert!(
            both.contains("robot@cn-a#11112222") && both.contains("robot@cn-a#aaaabbbb"),
            "each lifetime carries its own incarnation:\n{both}"
        );

        let one = render_table_output(
            &gathered(
                vec![hosted(domain("robot", "cn-a", FROZEN), "sim_1")],
                Vec::new(),
                Vec::new(),
            ),
            false,
            None,
        );
        assert!(
            one.contains("robot@cn-a") && !one.contains("robot@cn-a#"),
            "a single lifetime reads as name@core_node:\n{one}"
        );
    }

    #[test]
    fn owner_names_the_launch_the_daemon_reports_or_the_command() {
        let mut info = hosted(domain("robot", "cn-a", 7), "sim_1");
        assert_eq!(owner(&info), "peppy node run");

        info.launch = Some(LaunchIdentity::new("launch-1", "cn-sim"));
        assert_eq!(owner(&info), "launch launch-1 (cn-sim)");
    }

    #[test]
    fn readiness_reads_waiting_until_the_first_tick() {
        let mut info = hosted(domain("robot", "cn-a", 7), "sim_1");
        info.ready = false;
        assert_eq!(readiness(&info), "waiting");
        info.ready = true;
        assert_eq!(readiness(&info), "ready");
    }

    /// A domain is read wherever its consumers run, so the count covers every
    /// machine that answered.
    #[test]
    fn consumer_count_counts_every_machine() {
        let robot = domain("robot", "cn-a", 7);
        let listing = gathered(
            vec![hosted(robot.clone(), "sim_1")],
            vec![
                reader("cn-a", "arm_1", robot.clone()),
                reader("cn-b", "arm_2", robot.clone()),
                reader("cn-b", "other_1", domain("robot", "cn-b", 7)),
            ],
            Vec::new(),
        );
        assert_eq!(consumer_count(&listing, &listing.domains[0]), 2);
    }
}
