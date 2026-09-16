//! The rules a launcher's clock domains are held to, and the rule that a
//! clock-dependent connection joins two instances reading one clock.

mod common;

use common::{fragment_file, words, write};
use config::runtime::{
    BoundProducers, ClockBinding, ClockDomainId, ClockIncarnation, CoreNodeName, Name, ProducerRef,
    SlotBindings,
};
use daemon_config::launcher::{
    AppliedChange, ClockIncarnations, CompositionError, PeppyLauncherParser, Placements,
    PlannedObservation, PlannedPairEndpoint, PlannedPairing, PreparedLauncher, ResolvedClocks,
    resolve_clocks, validate_clock_connections,
};
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::tempdir;

const MACHINE: &str = "cn-test";

/// Every instance on the one machine a launch was sent to.
fn placements() -> Placements {
    Placements::all_on(CoreNodeName::new(MACHINE).expect("valid machine name"))
}

/// A flat launcher resolved as a launch on one machine would resolve it.
fn resolve(document: &str) -> Result<ResolvedClocks, Vec<String>> {
    let launcher = PeppyLauncherParser::from_content(document).expect("the document parses");
    resolve_clocks(&launcher, &placements(), &ClockIncarnations::new())
        .map_err(|errors| errors.iter().map(ToString::to_string).collect())
}

fn refusal(document: &str) -> String {
    resolve(document)
        .err()
        .map(|errors| errors.join("\n"))
        .unwrap_or_else(|| panic!("this document must be refused:\n{document}"))
}

/// Two instances of one node, so every case below has something to bind.
fn document(framework: &str, instances: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "launcher/v1",
            {framework}
            deployments: [
                {{ source: {{ name: "robot", tag: "v1" }}, instances: [{instances}] }},
            ],
        }}"#
    )
}

/// A launcher on disk, held ready to compose as `stack launch` holds it.
fn prepare(directory: &Path, document: &str) -> PreparedLauncher {
    let file = write(&directory.join("fleet.json5"), document);
    let parsed = PeppyLauncherParser::from_content(document).expect("the launcher parses");
    PreparedLauncher::load(&parsed, &file).expect("the fragments load")
}

/// Every refusal of a set of connections, as one message.
fn connection_refusals(errors: &[daemon_config::ParsingError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Naming an instance as a domain's publisher assigns it that domain and the
/// role together, so its entry carries no binding of its own.
#[test]
fn a_publisher_declaration_alone_assigns_the_domain() {
    let clocks = resolve(&document(
        r#"framework: { clocks: { simulated_robot: { publisher: "sim_inst" } } },"#,
        r#"{ instance_id: "sim_inst" }, { instance_id: "arm_inst", framework: { clock: "simulated_robot" } }"#,
    ))
    .expect("the document resolves");

    assert!(clocks.of("sim_inst").is_publisher());
    assert_eq!(
        clocks.of("sim_inst").label(),
        format!("simulated_robot@{MACHINE}")
    );
    assert_eq!(
        clocks.of("arm_inst").publisher_ref(),
        Some(&ProducerRef::new(MACHINE, "sim_inst")),
        "a consumer addresses the instance that supplies its domain"
    );
    assert!(
        clocks
            .of("sim_inst")
            .is_compatible_with(clocks.of("arm_inst")),
        "a publisher and its consumer read one timeline"
    );
}

/// An instance that names no clock reads its own machine's.
#[test]
fn an_omitted_binding_is_wall_time() {
    let clocks = resolve(&document("", r#"{ instance_id: "cam_inst" }"#)).expect("resolves");
    assert!(clocks.of("cam_inst").is_wall());
    assert!(clocks.of("never_deployed").is_wall());
}

/// Source ownership is declared once. A binding beside the declaration is a
/// second place to say the same thing, so it is refused even when it agrees.
#[test]
fn a_publisher_that_also_binds_a_clock_is_refused() {
    let message = refusal(&document(
        r#"framework: { clocks: { robot: { publisher: "sim_inst" } } },"#,
        r#"{ instance_id: "sim_inst", framework: { clock: "robot" } }"#,
    ));
    assert!(message.contains("sim_inst"), "{message}");
    assert!(message.contains("drop the binding"), "{message}");
}

/// An instance reads one clock, so it cannot supply two.
#[test]
fn one_instance_cannot_publish_two_domains() {
    let message = refusal(&document(
        r#"framework: { clocks: {
            first: { publisher: "sim_inst" },
            second: { publisher: "sim_inst" },
        } },"#,
        r#"{ instance_id: "sim_inst" }"#,
    ));
    assert!(
        message.contains("`first`") && message.contains("`second`"),
        "{message}"
    );
    assert!(message.contains("own publisher"), "{message}");
}

/// An unknown domain is an error, never a quiet fall back to wall time.
#[test]
fn an_unknown_domain_is_refused_rather_than_defaulted() {
    let message = refusal(&document(
        "",
        r#"{ instance_id: "arm_inst", framework: { clock: "simulated_robot" } }"#,
    ));
    assert!(message.contains("simulated_robot"), "{message}");
    assert!(message.contains("no `framework.clocks` entry"), "{message}");
}

/// A domain must name an instance the launch actually deploys. A launch that
/// deploys nothing is told to deploy the publisher it names.
#[test]
fn a_publisher_the_launch_does_not_deploy_is_refused() {
    let message = refusal(&document(
        r#"framework: { clocks: { robot: { publisher: "absent_inst" } } },"#,
        r#"{ instance_id: "arm_inst" }"#,
    ));
    assert!(message.contains("absent_inst"), "{message}");
    assert!(message.contains("does not deploy"), "{message}");
    assert!(message.contains("`arm_inst`"), "{message}");

    let empty = refusal(
        r#"{
            peppy_schema: "launcher/v1",
            framework: { clocks: { robot: { publisher: "sim_inst" } } },
            deployments: [],
        }"#,
    );
    assert!(empty.contains("deploys no instances"), "{empty}");
    assert!(
        empty.contains("Deploy `sim_inst` under `deployments`"),
        "the refusal says what to type: {empty}"
    );
}

/// `wall` names the time every machine already keeps, so declaring it could
/// only redefine it. An alias is how a launcher gives it a local name.
#[test]
fn wall_is_reserved_but_may_be_aliased() {
    let message = refusal(&document(
        r#"framework: { clocks: { wall: "wall" } },"#,
        r#"{ instance_id: "arm_inst" }"#,
    ));
    assert!(message.contains("built in"), "{message}");

    let clocks = resolve(&document(
        r#"framework: { clocks: { physical_robot: "wall" } },"#,
        r#"{ instance_id: "arm_inst", framework: { clock: "physical_robot" } }"#,
    ))
    .expect("an alias resolves");
    assert!(clocks.of("arm_inst").is_wall());
}

/// Two names for wall time are one timeline; two simulations are never one,
/// however their instants happen to line up.
#[test]
fn wall_aliases_agree_and_simulations_never_do() {
    let clocks = resolve(&document(
        r#"framework: { clocks: {
            physical_robot: "wall",
            bench: "wall",
            left_sim: { publisher: "left_sim_inst" },
            right_sim: { publisher: "right_sim_inst" },
        } },"#,
        r#"{ instance_id: "left_sim_inst" }, { instance_id: "right_sim_inst" },
           { instance_id: "a", framework: { clock: "physical_robot" } },
           { instance_id: "b", framework: { clock: "bench" } }"#,
    ))
    .expect("resolves");

    assert!(
        clocks.of("a").is_compatible_with(clocks.of("b")),
        "aliases of wall time are one timeline"
    );
    assert!(
        !clocks
            .of("left_sim_inst")
            .is_compatible_with(clocks.of("right_sim_inst")),
        "two simulations are two timelines"
    );
    assert!(
        !clocks
            .of("a")
            .is_compatible_with(clocks.of("left_sim_inst")),
        "wall time is not a simulation"
    );
}

/// Two documents may declare one domain, and identical declarations agree: a
/// robot fragment and the simulation it runs in can both name the same
/// timeline. Declarations that differ are refused naming both documents.
#[test]
fn two_documents_declaring_one_domain_must_agree() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("mujoco.json5"),
        &fragment_file(
            r#"framework: { clocks: { simulation: { publisher: "engine_inst" } } },
               deployments: [{ source: { name: "engine", tag: "v1" },
                               instances: [{ instance_id: "engine_inst" }] }]"#,
        ),
    );
    let launcher = |clocks: &str| {
        format!(
            r#"{{
                peppy_schema: "launcher/v1",
                framework: {{ clocks: {clocks} }},
                components: [
                    {{ name: "engine", cardinality: "zero_or_one",
                       options: {{ mujoco: "mujoco.json5" }} }},
                ],
                deployments: [],
            }}"#
        )
    };

    let error = prepare(directory.path(), &launcher(r#"{ simulation: "wall" }"#))
        .launch(&words(&["mujoco"]))
        .expect_err("one domain declared two ways must be refused");
    let CompositionError::ClockDomainConflict {
        domain,
        first_origin,
        second_origin,
        ..
    } = &error
    else {
        panic!("expected ClockDomainConflict, got: {error}");
    };
    assert_eq!(domain, "simulation");
    assert!(first_origin.contains("fleet.json5"), "{first_origin}");
    assert!(second_origin.contains("mujoco.json5"), "{second_origin}");
    let message = error.to_string();
    assert!(
        message.contains("\"wall\"") && message.contains("publisher: \"engine_inst\""),
        "the refusal quotes both declarations: {message}"
    );

    let composed = prepare(
        directory.path(),
        &launcher(r#"{ simulation: { publisher: "engine_inst" } }"#),
    )
    .launch(&words(&["mujoco"]))
    .expect("identical declarations agree");
    assert_eq!(
        composed
            .launcher
            .framework
            .clocks
            .keys()
            .map(Name::as_str)
            .collect::<Vec<_>>(),
        ["simulation"],
        "one domain, declared by both documents"
    );
}

/// An adjustment binds an instance's clock, and the report names the field
/// with the value it replaced. Two fragments binding one instance's clock are
/// two authors writing one field, which is refused.
#[test]
fn an_adjustment_binds_a_clock_and_two_fragments_cannot_both_bind_one() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("sim.json5"),
        &fragment_file(
            r#"adjustments: [{ target: "arm_inst", set_framework: { clock: "simulation" } }]"#,
        ),
    );
    write(
        &directory.path().join("bench.json5"),
        &fragment_file(
            r#"adjustments: [{ target: "arm_inst", set_framework: { clock: "wall" } }]"#,
        ),
    );
    let launcher = |option: &str| {
        format!(
            r#"{{
                peppy_schema: "launcher/v1",
                framework: {{ clocks: {{ simulation: {{ publisher: "engine_inst" }} }} }},
                components: [
                    {{ name: "engine", cardinality: "zero_or_one", options: {{ {option} }} }},
                ],
                deployments: [
                    {{ source: {{ name: "engine", tag: "v1" }},
                       instances: [{{ instance_id: "engine_inst" }}] }},
                    {{ source: {{ name: "arm", tag: "v1" }},
                       instances: [{{ instance_id: "arm_inst" }}] }},
                ],
            }}"#
        )
    };

    let composed = prepare(directory.path(), &launcher(r#"mujoco: "sim.json5""#))
        .launch(&words(&["mujoco"]))
        .expect("the adjustment applies");
    let bound = composed
        .report
        .applied
        .iter()
        .find(|entry| entry.target == "arm_inst")
        .expect("the fragment binds the arm's clock");
    assert!(
        matches!(&bound.change, AppliedChange::Clock { old: None, new } if new.as_str() == "simulation"),
        "{:?}",
        bound.change
    );
    let lines = composed.report.render_lines();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("arm_inst.framework.clock: (absent) -> simulation")),
        "{lines:?}"
    );
    let clocks = resolve_clocks(&composed.launcher, &placements(), &ClockIncarnations::new())
        .expect("the adjusted instance resolves");
    assert!(
        clocks
            .of("arm_inst")
            .is_compatible_with(clocks.of("engine_inst")),
        "the adjustment put the arm on the engine's timeline"
    );

    let error = prepare(
        directory.path(),
        &launcher(r#"mujoco: ["sim.json5", "bench.json5"]"#),
    )
    .launch(&words(&["mujoco"]))
    .expect_err("two fragments cannot both bind one clock");
    let CompositionError::AdjustmentsConflict { target, field, .. } = &error else {
        panic!("expected AdjustmentsConflict, got: {error}");
    };
    assert_eq!(
        (target.as_str(), field.as_str()),
        ("arm_inst", "framework.clock")
    );
}

/// A copy mints its instance ids under its own name, so a domain declared
/// inside one is refused naming the fragment that declares it. Binding a
/// domain the launcher declares is how a copy runs on a simulated timeline.
#[test]
fn a_copy_binds_a_launcher_domain_and_declares_none() {
    let directory = tempdir().unwrap();
    write(
        &directory.path().join("declares.json5"),
        &fragment_file(
            r#"framework: { clocks: { simulation: { publisher: "sim_inst" } } },
               deployments: [{ source: { name: "arm", tag: "v1" },
                               instances: [{ instance_id: "sim_inst" }] }]"#,
        ),
    );
    write(
        &directory.path().join("binds.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "arm", tag: "v1" },
                               instances: [{ instance_id: "arm_inst",
                                             framework: { clock: "simulation" } }] }]"#,
        ),
    );
    let launcher = |fragment: &str| {
        format!(
            r#"{{
                peppy_schema: "launcher/v1",
                framework: {{ clocks: {{ simulation: {{ publisher: "engine_inst" }} }} }},
                components: [
                    {{ name: "robot", cardinality: "zero_or_more",
                       options: {{ sim: "{fragment}" }} }},
                ],
                deployments: [
                    {{ source: {{ name: "engine", tag: "v1" }},
                       instances: [{{ instance_id: "engine_inst" }}] }},
                    {{ robot: "sim", instances: [{{ instance_id: "alpha" }}] }},
                ],
            }}"#
        )
    };

    let error = prepare(directory.path(), &launcher("declares.json5"))
        .launch(&[])
        .expect_err("a copy cannot declare a domain");
    let CompositionError::CopyFragmentDeclaresClock {
        origin,
        copy,
        domain,
    } = &error
    else {
        panic!("expected CopyFragmentDeclaresClock, got: {error}");
    };
    assert!(origin.contains("declares.json5"), "{origin}");
    assert_eq!((copy.as_str(), domain.as_str()), ("alpha", "simulation"));
    assert!(
        error.to_string().contains("framework.clocks"),
        "the refusal says where the domain belongs: {error}"
    );

    let composed = prepare(directory.path(), &launcher("binds.json5"))
        .launch(&[])
        .expect("a copy binds a domain the launcher declares");
    let clocks = resolve_clocks(&composed.launcher, &placements(), &ClockIncarnations::new())
        .expect("the copy's instance resolves");
    assert_eq!(
        clocks.of("alpha_arm_inst").publisher_ref(),
        Some(&ProducerRef::new(MACHINE, "engine_inst")),
        "the copy reads the launcher's domain"
    );
}

/// The connection rule, over each kind of clock-dependent connection: a
/// producer binding, a participant pairing and an observation.
#[test]
fn a_connection_across_two_clocks_is_refused_naming_both() {
    let clocks = resolve(&document(
        r#"framework: { clocks: { robot: { publisher: "sim_inst" } } },"#,
        r#"{ instance_id: "sim_inst" },
           { instance_id: "bound", framework: { clock: "robot" } },
           { instance_id: "unbound" }"#,
    ))
    .expect("resolves");

    let mut slot_bindings: BTreeMap<String, SlotBindings> = BTreeMap::new();
    slot_bindings.insert(
        "unbound".to_owned(),
        BTreeMap::from([(
            "camera".to_owned(),
            BoundProducers::try_from(vec![ProducerRef::new(MACHINE, "bound")])
                .expect("one producer is a valid set"),
        )]),
    );
    let pairings = [PlannedPairing {
        pairing_name: "arm_control".to_owned(),
        pairing_tag: "v1".to_owned(),
        a: PlannedPairEndpoint {
            instance_id: "unbound".to_owned(),
            link_id: "arm".to_owned(),
            role: "commander".to_owned(),
        },
        b: PlannedPairEndpoint {
            instance_id: "bound".to_owned(),
            link_id: "commander".to_owned(),
            role: "arm".to_owned(),
        },
    }];
    let observations = [PlannedObservation {
        observer_instance_id: "unbound".to_owned(),
        observer_link_id: "watched_arm".to_owned(),
        pairing_name: "arm_control".to_owned(),
        pairing_tag: "v1".to_owned(),
        observed_role: "arm".to_owned(),
        source: ProducerRef::new(MACHINE, "bound"),
        source_link_id: "commander".to_owned(),
    }];

    let errors = validate_clock_connections(&clocks, &slot_bindings, &pairings, &observations);
    let message = connection_refusals(&errors);
    assert_eq!(errors.len(), 3, "{message}");
    assert!(
        message.contains("`unbound`") && message.contains("`bound`"),
        "{message}"
    );
    assert!(
        message.contains("wall") && message.contains("robot@"),
        "{message}"
    );
    assert!(message.contains("binding `camera`"), "{message}");
    assert!(message.contains("pairing `arm`"), "{message}");
    assert!(message.contains("observation `watched_arm`"), "{message}");
}

/// Endpoints on one timeline connect freely, which is the case every
/// simulated launcher is written to hit.
#[test]
fn a_connection_within_one_clock_holds() {
    let clocks = resolve(&document(
        r#"framework: { clocks: { robot: { publisher: "sim_inst" } } },"#,
        r#"{ instance_id: "sim_inst" },
           { instance_id: "arm_inst", framework: { clock: "robot" } }"#,
    ))
    .expect("resolves");

    let slot_bindings: BTreeMap<String, SlotBindings> = BTreeMap::from([(
        "arm_inst".to_owned(),
        BTreeMap::from([(
            "simulation".to_owned(),
            BoundProducers::try_from(vec![ProducerRef::new(MACHINE, "sim_inst")])
                .expect("one producer is a valid set"),
        )]),
    )]);

    assert!(
        validate_clock_connections(&clocks, &slot_bindings, &[], &[]).is_empty(),
        "one domain carries its own connections"
    );
}

/// The clocks of instances already running, which a `node run` preflight
/// holds a new instance's connections against.
#[test]
fn running_instances_bring_their_own_clocks() {
    let simulation = ClockDomainId::new(
        Name::new("simulation").expect("a valid domain name"),
        CoreNodeName::new(MACHINE).expect("valid machine name"),
        ClockIncarnation::try_from(7).expect("non-zero"),
    );
    let clocks = ResolvedClocks::of_running([
        ("sim_inst".to_owned(), ClockBinding::publisher(simulation)),
        ("fresh_inst".to_owned(), ClockBinding::Wall),
    ]);
    assert!(clocks.of("fresh_inst").is_wall());

    let slot_bindings: BTreeMap<String, SlotBindings> = BTreeMap::from([(
        "fresh_inst".to_owned(),
        BTreeMap::from([(
            "simulation".to_owned(),
            BoundProducers::try_from(vec![ProducerRef::new(MACHINE, "sim_inst")])
                .expect("one producer is a valid set"),
        )]),
    )]);

    let errors = validate_clock_connections(&clocks, &slot_bindings, &[], &[]);
    let message = connection_refusals(&errors);
    assert_eq!(errors.len(), 1, "{message}");
    assert!(
        message.contains(&format!("simulation@{MACHINE}")) && message.contains("wall"),
        "a running publisher keeps its domain: {message}"
    );
}
