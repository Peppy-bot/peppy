//! The robots of a per-robot surface.
//!
//! The host reads the members the stack binds to each target and reports
//! them through a [`FleetSource`]; the server groups them by robot, routes
//! each call to the member the robot fills the target with, lists the fleet
//! through the listing tool, and publishes one resource per robot and member.
//! The source is read on every list and call, so a robot is addressable the
//! moment the stack binds it and gone the moment the stack drops it.

use crate::state::ResourceState;
use indexmap::IndexMap;
use peppy_mcp_catalog::{
    ExposureBundle, ROBOT_ARGUMENT, ResourceEntry, RobotCatalog, RobotContractPin,
};
use rmcp::model::Resource;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

/// The wire address of one member of a target's bound set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberAddress {
    pub core_node: String,
    pub instance_id: String,
}

/// One member of a target, as the host reads it from the stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FleetMember {
    /// The target (the contract slot's `link_id`) the member fills.
    pub target: String,
    pub address: MemberAddress,
    /// The robot the member belongs to: the name of its copy. `None` for an
    /// instance the launcher deploys outside any copy, which serves no
    /// robot.
    pub robot: Option<String>,
    /// The member's name within its robot, which a call names it by on a
    /// target a robot fills any number of times.
    pub name: String,
}

/// Reads the members of every target as the stack binds them now.
pub trait FleetSource: Send + Sync + 'static {
    fn members(&self) -> Vec<FleetMember>;
}

impl<F> FleetSource for F
where
    F: Fn() -> Vec<FleetMember> + Send + Sync + 'static,
{
    fn members(&self) -> Vec<FleetMember> {
        self()
    }
}

/// What a robot fills one target with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fill {
    /// A target filled once per robot.
    Once(MemberAddress),
    /// A target a robot fills any number of times, each member named by
    /// the target's argument.
    Many {
        argument: String,
        named: IndexMap<String, MemberAddress>,
    },
    /// A target filled once per robot that this robot fills more than once,
    /// which the stack's plan allows and this surface cannot serve.
    Conflict(Vec<String>),
}

/// One robot: the targets it fills, in the order its members were read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Robot {
    fills: IndexMap<String, Fill>,
}

/// The fleet as the source reports it, grouped by robot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fleet {
    robots: IndexMap<String, Robot>,
    /// Members the surface cannot serve: each names the member and why.
    problems: Vec<String>,
}

/// Why a call cannot reach a member. Rendered for the client, naming what
/// is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteRefusal {
    NoSuchRobot {
        robot: String,
        robots: Vec<String>,
    },
    TargetUnfilled {
        robot: String,
        target: String,
        filled: Vec<String>,
        /// The robots of the stack that fill the target.
        robots_with: Vec<String>,
    },
    NoSuchMember {
        robot: String,
        target: String,
        argument: String,
        name: String,
        names: Vec<String>,
    },
    /// A call on a target filled any number of times named no member.
    MemberUnnamed {
        robot: String,
        target: String,
        argument: String,
        names: Vec<String>,
    },
    Conflict {
        robot: String,
        target: String,
        instances: Vec<String>,
    },
}

impl std::fmt::Display for RouteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchRobot { robot, robots } if robots.is_empty() => {
                write!(
                    f,
                    "`{robot}` is not a robot of this stack, which has no robot; `peppy stack \
                     join OPTION:NAME` adds one"
                )
            }
            Self::NoSuchRobot { robot, robots } => write!(
                f,
                "`{robot}` is not a robot of this stack; the robots are {}",
                quoted(robots)
            ),
            Self::TargetUnfilled {
                robot,
                target,
                filled,
                robots_with,
            } => {
                write!(f, "robot `{robot}` has no `{target}`; it fills ")?;
                match filled.as_slice() {
                    [] => write!(f, "no target")?,
                    filled => write!(f, "{}", quoted(filled))?,
                }
                match robots_with.as_slice() {
                    [] => write!(f, ", and no robot of this stack has a `{target}`"),
                    robots => write!(f, "; the robots with a `{target}` are {}", quoted(robots)),
                }
            }
            Self::NoSuchMember {
                robot,
                target,
                argument,
                name,
                names,
            } => write!(
                f,
                "robot `{robot}` has no `{target}` named `{name}` (`{argument}`); it has {}",
                quoted(names)
            ),
            Self::MemberUnnamed {
                robot,
                target,
                argument,
                names,
            } => write!(
                f,
                "`{target}` of robot `{robot}` is filled by several members; name one with \
                 `{argument}`: {}",
                quoted(names)
            ),
            Self::Conflict {
                robot,
                target,
                instances,
            } => write!(
                f,
                "robot `{robot}` fills `{target}` with {} instances ({}), and a target filled \
                 once per robot cannot serve two; fix the launcher",
                instances.len(),
                quoted(instances)
            ),
        }
    }
}

pub(crate) fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Fleet {
    /// Groups `members` by robot. `argument_of` says which targets a robot
    /// fills any number of times.
    pub(crate) fn group(
        members: Vec<FleetMember>,
        argument_of: &HashMap<String, Option<String>>,
    ) -> Self {
        let mut robots: IndexMap<String, Robot> = IndexMap::new();
        let mut problems = Vec::new();
        for member in members {
            let Some(robot) = member.robot else {
                problems.push(format!(
                    "`{}` fills `{}` outside any copy, and only a robot's copy serves it",
                    member.address.instance_id, member.target
                ));
                continue;
            };
            let Some(argument) = argument_of.get(&member.target) else {
                problems.push(format!(
                    "`{}` fills `{}`, which the surface does not declare",
                    member.address.instance_id, member.target
                ));
                continue;
            };
            let fills = &mut robots
                .entry(robot)
                .or_insert(Robot {
                    fills: IndexMap::new(),
                })
                .fills;
            match (argument, fills.get_mut(&member.target)) {
                (Some(_), Some(Fill::Many { named, .. })) => {
                    named.insert(member.name, member.address);
                }
                (Some(argument), None) => {
                    fills.insert(
                        member.target,
                        Fill::Many {
                            argument: argument.clone(),
                            named: IndexMap::from([(member.name, member.address)]),
                        },
                    );
                }
                (None, None) => {
                    fills.insert(member.target, Fill::Once(member.address));
                }
                (None, Some(fill)) => {
                    let mut instances = match fill {
                        Fill::Once(first) => vec![first.instance_id.clone()],
                        Fill::Conflict(instances) => std::mem::take(instances),
                        Fill::Many { .. } => unreachable!("a target has one argument or none"),
                    };
                    instances.push(member.address.instance_id);
                    *fill = Fill::Conflict(instances);
                }
                (Some(_), Some(_)) => unreachable!("a target has one argument or none"),
            }
        }
        for (robot, entry) in &robots {
            for (target, fill) in &entry.fills {
                if let Fill::Conflict(instances) = fill {
                    problems.push(
                        RouteRefusal::Conflict {
                            robot: robot.clone(),
                            target: target.clone(),
                            instances: instances.clone(),
                        }
                        .to_string(),
                    );
                }
            }
        }
        Self { robots, problems }
    }

    /// The members the surface cannot serve, each with the reason: a member
    /// outside any copy, one on a target the surface does not declare, and
    /// the members of a robot filling a once-target more than once.
    pub(crate) fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The robots, in the order of the surface's targets and, within one,
    /// of the members the stack binds to it.
    pub(crate) fn robot_names(&self) -> Vec<String> {
        self.robots.keys().cloned().collect()
    }

    /// The member `robot` fills `target` with, named by `name` on a target
    /// filled any number of times.
    pub(crate) fn route(
        &self,
        robot: &str,
        target: &str,
        name: Option<&str>,
    ) -> Result<MemberAddress, RouteRefusal> {
        let Some(entry) = self.robots.get(robot) else {
            return Err(RouteRefusal::NoSuchRobot {
                robot: robot.to_string(),
                robots: self.robot_names(),
            });
        };
        let Some(fill) = entry.fills.get(target) else {
            return Err(RouteRefusal::TargetUnfilled {
                robot: robot.to_string(),
                target: target.to_string(),
                filled: entry.fills.keys().cloned().collect(),
                robots_with: self
                    .robots
                    .iter()
                    .filter(|(_, other)| other.fills.contains_key(target))
                    .map(|(name, _)| name.clone())
                    .collect(),
            });
        };
        match (fill, name) {
            (Fill::Once(address), _) => Ok(address.clone()),
            (Fill::Many { argument, named }, Some(name)) => {
                named
                    .get(name)
                    .cloned()
                    .ok_or_else(|| RouteRefusal::NoSuchMember {
                        robot: robot.to_string(),
                        target: target.to_string(),
                        argument: argument.clone(),
                        name: name.to_string(),
                        names: named.keys().cloned().collect(),
                    })
            }
            (Fill::Many { argument, named }, None) => Err(RouteRefusal::MemberUnnamed {
                robot: robot.to_string(),
                target: target.to_string(),
                argument: argument.clone(),
                names: named.keys().cloned().collect(),
            }),
            (Fill::Conflict(instances), _) => Err(RouteRefusal::Conflict {
                robot: robot.to_string(),
                target: target.to_string(),
                instances: instances.clone(),
            }),
        }
    }

    /// Every resource the fleet publishes, one per robot and member for each
    /// resource entry of a filled target, in robot order.
    pub(crate) fn resources(&self, entries: &[ResourceEntry]) -> Vec<Resource> {
        let mut listed = Vec::new();
        for (robot, fills) in &self.robots {
            for entry in entries {
                match fills.fills.get(&entry.target) {
                    Some(Fill::Once(_)) => listed.push(published_resource(entry, robot, None)),
                    Some(Fill::Many { named, .. }) => listed.extend(
                        named
                            .keys()
                            .map(|name| published_resource(entry, robot, Some(name))),
                    ),
                    Some(Fill::Conflict(_)) | None => {}
                }
            }
        }
        listed
    }

    /// One robot's listing entry without its `describe` values: the tools it
    /// answers and the resources it publishes, both sorted, its named
    /// members and the problems the surface has with it.
    pub(crate) fn listing_entry(
        &self,
        robot: &str,
        by_target: &HashMap<String, TargetNames>,
    ) -> Option<Value> {
        let entry = self.robots.get(robot)?;
        let mut tools: Vec<&str> = Vec::new();
        let mut resources: Vec<String> = Vec::new();
        let mut members = serde_json::Map::new();
        let mut notes = Vec::new();
        for (target, fill) in &entry.fills {
            let filled_by: Vec<Option<&str>> = match fill {
                Fill::Once(_) => vec![None],
                Fill::Many { named, .. } => {
                    members.insert(
                        target.clone(),
                        Value::Array(named.keys().map(|name| json!(name)).collect()),
                    );
                    named.keys().map(|name| Some(name.as_str())).collect()
                }
                Fill::Conflict(instances) => {
                    notes.push(json!(
                        RouteRefusal::Conflict {
                            robot: robot.to_string(),
                            target: target.clone(),
                            instances: instances.clone(),
                        }
                        .to_string()
                    ));
                    continue;
                }
            };
            let Some(names) = by_target.get(target) else {
                continue;
            };
            tools.extend(names.tools.iter().map(String::as_str));
            resources.extend(names.resources.iter().flat_map(|resource| {
                filled_by
                    .iter()
                    .map(move |name| published_name(resource, robot, *name))
            }));
        }
        tools.sort_unstable();
        resources.sort_unstable();
        Some(json!({
            ROBOT_ARGUMENT: robot,
            "tools": tools,
            "resources": resources,
            "members": members,
            "notes": notes,
        }))
    }
}

/// The name a resource of `entry` is published under for `robot`, and for
/// its member `name` on a target filled any number of times.
fn published_name(entry: &ResourceEntry, robot: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!("{robot}/{name}/{}", entry.name),
        None => format!("{robot}/{}", entry.name),
    }
}

/// The URI of a published resource, from its published name.
fn published_uri(name: &str) -> String {
    format!("peppy://resource/{name}")
}

fn published_resource(entry: &ResourceEntry, robot: &str, name: Option<&str>) -> Resource {
    let published = published_name(entry, robot, name);
    Resource::new(published_uri(&published), published)
        .with_description(entry.description.clone())
        .with_mime_type("application/json")
}

/// The published resources of one member, each with its runtime state.
pub(crate) type MemberResources = Vec<(ResourceEntry, Arc<ResourceState>)>;

/// What one target publishes, in catalog order: the tools whose calls it
/// takes, and the resource entries published for each member filling it.
#[derive(Debug, Default)]
pub(crate) struct TargetNames {
    tools: Vec<String>,
    resources: Vec<ResourceEntry>,
}

/// The per-robot surface as the server holds it: the catalog, the source of
/// the fleet, and the resource states the host attached for the members
/// that run.
pub(crate) struct FleetRuntime {
    pub(crate) catalog: RobotCatalog,
    pub(crate) source: Arc<dyn FleetSource>,
    /// The argument a target's calls name a member by, keyed by target;
    /// `None` for a target filled once per robot.
    pub(crate) argument_of: HashMap<String, Option<String>>,
    /// What every target publishes, keyed by target.
    pub(crate) by_target: HashMap<String, TargetNames>,
    /// The resource entries of every target, flat, in catalog order.
    pub(crate) entries: Vec<ResourceEntry>,
    /// The state of every attached resource, keyed by URI.
    pub(crate) states: RwLock<HashMap<String, Arc<ResourceState>>>,
}

impl FleetRuntime {
    /// The runtime of `bundle`'s per-robot surface, whose `catalog` and
    /// `contracts` the caller took out of it.
    pub(crate) fn new(
        bundle: &ExposureBundle,
        catalog: RobotCatalog,
        contracts: &[RobotContractPin],
        source: Arc<dyn FleetSource>,
    ) -> Self {
        let argument_of = contracts
            .iter()
            .map(|pin| (pin.pin.link_id.clone(), pin.argument.clone()))
            .collect();
        let mut by_target: HashMap<String, TargetNames> = HashMap::new();
        for entry in &bundle.resources {
            by_target
                .entry(entry.target.clone())
                .or_default()
                .resources
                .push(entry.clone());
        }
        let tools = bundle
            .tools
            .iter()
            .map(|tool| (&tool.target, &tool.name))
            .chain(bundle.tasks.iter().map(|task| (&task.target, &task.name)));
        for (target, name) in tools {
            by_target
                .entry(target.clone())
                .or_default()
                .tools
                .push(name.clone());
        }
        Self {
            catalog,
            source,
            argument_of,
            by_target,
            entries: bundle.resources.clone(),
            states: RwLock::new(HashMap::new()),
        }
    }

    /// The fleet as the source reports it now.
    pub(crate) fn fleet(&self) -> Fleet {
        Fleet::group(self.source.members(), &self.argument_of)
    }

    /// The resource entries `target` publishes, in catalog order.
    fn published_by(&self, target: &str) -> &[ResourceEntry] {
        self.by_target
            .get(target)
            .map(|names| names.resources.as_slice())
            .unwrap_or_default()
    }

    /// The published name of `entry` for `member`.
    fn name_for(&self, member: &FleetMember, entry: &ResourceEntry) -> Option<String> {
        let robot = member.robot.as_deref()?;
        let name = self
            .argument_of
            .get(&member.target)?
            .as_ref()
            .map(|_| member.name.as_str());
        Some(published_name(entry, robot, name))
    }

    /// Registers the resource states of `member`'s target, returning each
    /// with its entry so the host can feed it. A member the fleet does not
    /// route to (serving no robot, or one of several filling a once-target),
    /// or a target with no resources, attaches nothing.
    pub(crate) fn attach(&self, member: &FleetMember) -> MemberResources {
        let entries = self.published_by(&member.target);
        if entries.is_empty() || !self.routes_to(member) {
            return Vec::new();
        }
        let mut states = self.states.write().expect("fleet lock is never poisoned");
        entries
            .iter()
            .filter_map(|entry| {
                let name = self.name_for(member, entry)?;
                let state = Arc::new(ResourceState::new(ResourceEntry {
                    name: name.clone(),
                    uri: published_uri(&name),
                    ..entry.clone()
                }));
                states.insert(published_uri(&name), Arc::clone(&state));
                Some((entry.clone(), state))
            })
            .collect()
    }

    /// Whether a call naming `member`'s robot and name reaches it.
    fn routes_to(&self, member: &FleetMember) -> bool {
        let Some(robot) = member.robot.as_deref() else {
            return false;
        };
        let Some(argument) = self.argument_of.get(&member.target) else {
            return false;
        };
        let name = argument.as_ref().map(|_| member.name.as_str());
        self.fleet()
            .route(robot, &member.target, name)
            .is_ok_and(|address| address == member.address)
    }

    /// Drops the resource states of `member`.
    pub(crate) fn detach(&self, member: &FleetMember) {
        let entries = self.published_by(&member.target);
        if entries.is_empty() {
            return;
        }
        let mut states = self.states.write().expect("fleet lock is never poisoned");
        for entry in entries {
            if let Some(name) = self.name_for(member, entry) {
                states.remove(&published_uri(&name));
            }
        }
    }

    pub(crate) fn state(&self, uri: &str) -> Option<Arc<ResourceState>> {
        self.states
            .read()
            .expect("fleet lock is never poisoned")
            .get(uri)
            .cloned()
    }

    /// The fields of the listing entry the `describe` sources fill, keyed
    /// by field, for the listing tool's output schema.
    pub(crate) fn describe_schema(&self) -> BTreeMap<String, Value> {
        self.catalog
            .describe
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    json!({ "description": format!("Read through the robot's `{}`; null when it could not be read, with the reason under `notes`.", entry.target) }),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(target: &str, instance_id: &str, robot: Option<&str>, name: &str) -> FleetMember {
        FleetMember {
            target: target.to_string(),
            address: MemberAddress {
                core_node: "cn".to_string(),
                instance_id: instance_id.to_string(),
            },
            robot: robot.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn arguments() -> HashMap<String, Option<String>> {
        HashMap::from([
            ("postures".to_string(), None),
            ("camera".to_string(), Some("camera".to_string())),
        ])
    }

    fn resource(name: &str, target: &str) -> ResourceEntry {
        serde_json::from_value(json!({
            "name": name,
            "uri": published_uri(name),
            "description": "The latest snapshot.",
            "target": target,
            "member": "snapshot",
            "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 2.0 } },
            "schema": { "type": "object" },
        }))
        .expect("a resource entry")
    }

    /// What each target of the fleet publishes: one tool and one resource.
    fn names() -> HashMap<String, TargetNames> {
        HashMap::from([
            (
                "postures".to_string(),
                TargetNames {
                    tools: vec!["robot.move_arm".to_string()],
                    resources: vec![resource("robot.status", "postures")],
                },
            ),
            (
                "camera".to_string(),
                TargetNames {
                    tools: vec!["camera.set_brightness".to_string()],
                    resources: vec![resource("camera.latest_frame", "camera")],
                },
            ),
        ])
    }

    fn fleet() -> Fleet {
        Fleet::group(
            vec![
                member("postures", "alpha_backbone", Some("alpha"), "backbone"),
                member("camera", "alpha_wrist_left", Some("alpha"), "wrist_left"),
                member("camera", "alpha_wrist_right", Some("alpha"), "wrist_right"),
                member("postures", "bravo_backbone", Some("bravo"), "backbone"),
            ],
            &arguments(),
        )
    }

    #[test]
    fn robots_keep_the_order_their_members_were_read_in() {
        assert_eq!(fleet().robot_names(), ["alpha", "bravo"]);
        assert!(fleet().problems().is_empty());
    }

    #[test]
    fn a_call_routes_to_the_member_the_robot_fills_the_target_with() {
        let fleet = fleet();
        assert_eq!(
            fleet.route("alpha", "postures", None).unwrap().instance_id,
            "alpha_backbone"
        );
        assert_eq!(
            fleet
                .route("alpha", "camera", Some("wrist_right"))
                .unwrap()
                .instance_id,
            "alpha_wrist_right"
        );
    }

    #[test]
    fn refusals_name_what_is_there() {
        let fleet = fleet();
        assert_eq!(
            fleet
                .route("charlie", "postures", None)
                .unwrap_err()
                .to_string(),
            "`charlie` is not a robot of this stack; the robots are `alpha`, `bravo`"
        );
        assert_eq!(
            fleet
                .route("bravo", "camera", Some("front"))
                .unwrap_err()
                .to_string(),
            "robot `bravo` has no `camera`; it fills `postures`; the robots with a `camera` are `alpha`"
        );
        assert_eq!(
            fleet
                .route("alpha", "camera", Some("front"))
                .unwrap_err()
                .to_string(),
            "robot `alpha` has no `camera` named `front` (`camera`); it has `wrist_left`, \
             `wrist_right`"
        );
        let empty = Fleet::group(Vec::new(), &arguments());
        assert_eq!(
            empty
                .route("alpha", "postures", None)
                .unwrap_err()
                .to_string(),
            "`alpha` is not a robot of this stack, which has no robot; `peppy stack join \
             OPTION:NAME` adds one"
        );
    }

    #[test]
    fn a_target_filled_twice_by_one_robot_is_a_conflict_and_a_member_of_no_copy_a_problem() {
        let fleet = Fleet::group(
            vec![
                member("postures", "alpha_backbone", Some("alpha"), "backbone"),
                member("postures", "alpha_other", Some("alpha"), "other"),
                member("postures", "stray_inst", None, "stray_inst"),
                member("brain", "alpha_brain", Some("alpha"), "brain"),
            ],
            &arguments(),
        );
        assert_eq!(
            fleet
                .route("alpha", "postures", None)
                .unwrap_err()
                .to_string(),
            "robot `alpha` fills `postures` with 2 instances (`alpha_backbone`, \
             `alpha_other`), and a target filled once per robot cannot serve two; fix the \
             launcher"
        );
        assert_eq!(
            fleet.problems(),
            [
                "`stray_inst` fills `postures` outside any copy, and only a robot's copy serves it",
                "`alpha_brain` fills `brain`, which the surface does not declare",
                "robot `alpha` fills `postures` with 2 instances (`alpha_backbone`, \
                 `alpha_other`), and a target filled once per robot cannot serve two; fix the \
                 launcher",
            ]
        );
        let entry = fleet.listing_entry("alpha", &names()).unwrap();
        assert_eq!(entry["tools"], json!([]));
        assert_eq!(entry["resources"], json!([]));
        assert_eq!(entry["notes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn the_listing_entry_names_the_tools_resources_and_members() {
        let entry = fleet().listing_entry("alpha", &names()).unwrap();
        assert_eq!(
            entry,
            json!({
                "robot": "alpha",
                "tools": ["camera.set_brightness", "robot.move_arm"],
                "resources": [
                    "alpha/robot.status",
                    "alpha/wrist_left/camera.latest_frame",
                    "alpha/wrist_right/camera.latest_frame",
                ],
                "members": { "camera": ["wrist_left", "wrist_right"] },
                "notes": [],
            })
        );
        assert_eq!(fleet().listing_entry("charlie", &names()), None);
    }

    #[test]
    fn resources_are_published_per_robot_and_member() {
        let entries = [
            resource("camera.latest_frame", "camera"),
            resource("robot.status", "postures"),
        ];
        let listed: Vec<(String, String)> = fleet()
            .resources(&entries)
            .iter()
            .map(|resource| (resource.name.to_string(), resource.uri.to_string()))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "alpha/wrist_left/camera.latest_frame".to_string(),
                    "peppy://resource/alpha/wrist_left/camera.latest_frame".to_string()
                ),
                (
                    "alpha/wrist_right/camera.latest_frame".to_string(),
                    "peppy://resource/alpha/wrist_right/camera.latest_frame".to_string()
                ),
                (
                    "alpha/robot.status".to_string(),
                    "peppy://resource/alpha/robot.status".to_string()
                ),
                (
                    "bravo/robot.status".to_string(),
                    "peppy://resource/bravo/robot.status".to_string()
                ),
            ]
        );
    }
}
