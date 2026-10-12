use super::*;
use crate::internal::mcp_exposure::PeppyMcpExposureParser;
use peppy_mcp_catalog::{ExposureBundle, build_exposure_bundle};
use serde_json::json;

/// The design's `framework/framework_controls.json5`: one fixed target,
/// `stack`, that names `stack_copies:v1`.
pub(crate) const FRAMEWORK_CONTROLS: &str = include_str!("fixtures/framework_controls.json5");

fn reference(name: &str, tag: &str) -> DaemonInterfaceRef {
    DaemonInterfaceRef {
        name: config::runtime::Name::new(name).expect("a name"),
        tag: tag.to_owned(),
    }
}

#[test]
fn the_compiled_in_document_parses_as_a_contract_of_the_registered_identity() {
    for interface in DaemonInterface::ALL {
        let parsed = PeppyContractParser::from_content(interface.document_text())
            .unwrap_or_else(|error| panic!("{} does not parse: {error}", interface.label()));
        assert_eq!(parsed.manifest.name.as_str(), interface.name());
        assert_eq!(parsed.manifest.tag, interface.tag());
        assert_eq!(&parsed, interface.document());
    }
    let stack_copies = DaemonInterface::StackCopies.document();
    let services: Vec<&str> = stack_copies
        .interfaces
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    let actions: Vec<&str> = stack_copies
        .interfaces
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(services, ["list"]);
    assert_eq!(actions, ["join", "remove"]);
    assert!(stack_copies.interfaces.topics.is_empty());
    assert!(
        DaemonInterface::StackCopies
            .document_text()
            .starts_with("// The copies of the running launch: list them, add one, remove one."),
        "the document keeps its comments"
    );
}

/// The members the bridge and the narrowing name are the members of the
/// compiled-in document, each of its kind.
#[test]
fn the_stack_copies_members_are_the_members_of_its_document() {
    let interfaces = &DaemonInterface::StackCopies.document().interfaces;
    let services: Vec<&str> = interfaces
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();
    let actions: Vec<&str> = interfaces
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(services, [StackCopiesMember::List.name()]);
    assert_eq!(
        actions,
        [
            StackCopiesMember::Join.name(),
            StackCopiesMember::Remove.name()
        ]
    );
    for member in StackCopiesMember::ALL {
        assert_eq!(StackCopiesMember::named(member.name()), Some(member));
    }
    assert_eq!(StackCopiesMember::named("reset"), None);
}

#[test]
fn the_registry_serves_one_tag_of_each_interface() {
    assert_eq!(
        DaemonInterface::served(&reference("stack_copies", "v1")),
        Ok(DaemonInterface::StackCopies)
    );

    let error =
        DaemonInterface::served(&reference("stack_copies", "v2")).expect_err("v2 is not served");
    assert_eq!(
        error.to_string(),
        "daemon interface `stack_copies:v2` is not one this peppy serves; it serves \
         `stack_copies` at tag `v1` only, and a document that names a daemon interface ships \
         with the peppy release that serves its tag"
    );

    let error =
        DaemonInterface::served(&reference("node_controls", "v1")).expect_err("no such interface");
    assert_eq!(
        error.to_string(),
        "daemon interface `node_controls:v1` is not one this peppy serves; it serves \
         `stack_copies:v1`"
    );
}

#[test]
fn the_daemon_targets_of_an_exposure_resolve_through_the_registry() {
    let exposure =
        PeppyMcpExposureParser::from_content(FRAMEWORK_CONTROLS).expect("the fixture parses");
    assert_eq!(
        served_interfaces(&exposure),
        Ok(vec![("stack".to_owned(), DaemonInterface::StackCopies)])
    );

    let unserved = PeppyMcpExposureParser::from_content(&FRAMEWORK_CONTROLS.replace(
        r#"daemon: { name: "stack_copies", tag: "v1" }"#,
        r#"daemon: { name: "stack_copies", tag: "v9" }"#,
    ))
    .expect("the document parses whatever tag it names");
    let refusals = served_interfaces(&unserved).expect_err("v9 is not served");
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals[0].starts_with(
            "target `stack`: daemon interface `stack_copies:v9` is not one this peppy serves; \
             it serves `stack_copies` at tag `v1` only"
        ),
        "{refusals:?}"
    );
}

fn scope(value: serde_json::Value) -> Result<StackCopiesScope, String> {
    match DaemonInterface::StackCopies.parse_scope(&value)? {
        DaemonScope::StackCopies(scope) => Ok(scope),
    }
}

fn simulation_scope() -> serde_json::Value {
    json!({
        "max_copies": 4,
        "options": [
            {
                "option": "openarm_sim",
                "description": "A simulated OpenArm v2 standing on the floor",
                "features": [{
                    "feature": "ai_brain",
                    "axis": "brain",
                    "option": "ai_brain_vla",
                    "description": "An AI brain that finds items by words, then grabs and places them",
                }],
            },
            { "option": "so101_sim", "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

/// A scope whose options offer no feature.
fn featureless_scope() -> serde_json::Value {
    json!({
        "max_copies": 4,
        "options": [
            { "option": "openarm_sim", "description": "A simulated OpenArm v2 standing on the floor" },
            { "option": "so101_sim", "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

#[test]
fn a_stack_copies_scope_parses_into_its_type() {
    let parsed = scope(simulation_scope()).expect("parses");
    let options: Vec<(&str, &str, Vec<&str>)> = parsed
        .options()
        .iter()
        .map(|entry| {
            (
                entry.option().as_str(),
                entry.description().as_str(),
                entry
                    .features()
                    .iter()
                    .map(|feature| feature.feature().as_str())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        options,
        [
            (
                "openarm_sim",
                "A simulated OpenArm v2 standing on the floor",
                vec!["ai_brain"]
            ),
            (
                "so101_sim",
                "A simulated SO-101 clamped on the edge of a table",
                vec![]
            ),
        ]
    );
    let brain = parsed
        .option("openarm_sim")
        .and_then(|entry| entry.feature("ai_brain"))
        .expect("the feature");
    assert_eq!(
        (brain.axis().as_str(), brain.option().as_str()),
        ("brain", "ai_brain_vla")
    );
    assert_eq!(parsed.max_copies().get(), 4);
    assert_eq!(
        DaemonInterface::StackCopies
            .parse_scope(&simulation_scope())
            .expect("parses")
            .interface(),
        DaemonInterface::StackCopies
    );
}

/// A feature's word names the axis, and its machine; the features a robot
/// holds are the features its selections fill, in the scope's order, each
/// with the machine of its part.
#[test]
fn a_feature_gives_its_word_and_is_found_in_a_robots_selections() {
    let parsed = scope(json!({
        "max_copies": 4,
        "options": [{
            "option": "openarm_sim",
            "description": "An OpenArm",
            "features": [
                { "feature": "recorder", "axis": "recorder", "option": "lerobot_recorder", "description": "Records." },
                { "feature": "ai_brain", "axis": "brain", "option": "ai_brain_vla", "description": "Thinks." },
            ],
        }],
    }))
    .expect("parses");
    let openarm = parsed.option("openarm_sim").expect("the option");
    let brain = openarm.feature("ai_brain").expect("the feature");
    assert_eq!(brain.word(None).to_string(), "brain=ai_brain_vla");
    assert_eq!(
        brain.word(Some("cn-gpu-box".parse().unwrap())).to_string(),
        "brain=ai_brain_vla@cn-gpu-box"
    );
    assert_eq!(
        brain
            .word(Some(config::runtime::CoreNodeTarget::Coordinator))
            .to_string(),
        "brain=ai_brain_vla@self"
    );

    let selections: Vec<core_node_api::encoding::AxisSelection> = [
        "hardware_version=v2",
        "brain=ai_brain_vla@cn-gpu-box",
        "recorder=lerobot_recorder",
    ]
    .iter()
    .map(|text| text.parse().unwrap())
    .collect();
    let held: Vec<(&str, Option<&str>)> = openarm
        .features_in(&selections)
        .iter()
        .map(|placed| {
            (
                placed.feature.feature().as_str(),
                placed.core_node.map(config::runtime::CoreNodeName::as_str),
            )
        })
        .collect();
    assert_eq!(
        held,
        [("recorder", None), ("ai_brain", Some("cn-gpu-box"))],
        "in the scope's order, whatever the order of the selections"
    );
    let other_option: Vec<core_node_api::encoding::AxisSelection> =
        vec!["brain=other_brain".parse().unwrap()];
    assert!(openarm.features_in(&other_option).is_empty());
}

#[test]
fn a_stack_copies_scope_holds_its_rules() {
    let option =
        |option: &str, description: &str| json!({ "option": option, "description": description });
    let refused = |value: serde_json::Value| scope(value).expect_err("the scope is refused");

    let error = refused(json!({ "options": [], "max_copies": 1 }));
    assert!(error.contains("`options` names no option"), "{error}");

    let error = refused(json!({
        "options": [option("a", "One."), option("a", "Again.")],
        "max_copies": 1,
    }));
    assert!(error.contains("names option `a` more than once"), "{error}");

    let error = refused(json!({ "options": [option("bad option", "One.")], "max_copies": 1 }));
    assert!(error.starts_with("options[0].option:"), "{error}");

    let error = refused(json!({ "options": [option("a", "  ")], "max_copies": 1 }));
    assert_eq!(
        error,
        "the description of option `a` is empty; it says in one line what it is"
    );
    let error = refused(json!({ "options": [option("a", "One.\nTwo.")], "max_copies": 1 }));
    assert!(
        error.starts_with("the description of option `a` is one line"),
        "{error}"
    );
    let long = "é".repeat(MAX_DESCRIPTION_CHARS + 1);
    let error = refused(json!({ "options": [option("a", &long)], "max_copies": 1 }));
    assert_eq!(
        error,
        "the description of option `a` is at most 200 characters, and this one has 201"
    );
    let longest = "é".repeat(MAX_DESCRIPTION_CHARS);
    scope(json!({ "options": [option("a", &longest)], "max_copies": 1 }))
        .expect("200 characters fit, counted as characters and not bytes");

    for max_copies in [json!(0), json!(65536), json!(-1), json!(1.5)] {
        let error = refused(json!({ "options": [option("a", "One.")], "max_copies": max_copies }));
        assert!(
            error.contains("`max_copies` is a whole number from 1 to 65535"),
            "{max_copies}: {error}"
        );
    }
    assert_eq!(
        scope(json!({ "options": [option("a", "One.")], "max_copies": 65535 }))
            .expect("the largest u16")
            .max_copies()
            .get(),
        65535
    );

    let error = refused(json!({ "options": [option("a", "One.")] }));
    assert!(error.contains("max_copies"), "{error}");
    let error = refused(json!({ "options": [option("a", "One.")], "max_copies": 1, "model": "x" }));
    assert!(error.contains("unknown field `model`"), "{error}");
    let error = refused(json!({
        "options": [{ "option": "a", "description": "One.", "model": "so101" }],
        "max_copies": 1,
    }));
    assert!(
        error.starts_with("options[0]") && error.contains("unknown field `model`"),
        "{error}"
    );
    let error = refused(json!("openarm_sim"));
    assert!(!error.is_empty());
}

/// The rules of the features of an option: present means at least one, at
/// most six, each with known keys, its own name, its own axis and a
/// description that holds the rules of every description.
#[test]
fn the_features_of_an_option_hold_their_rules() {
    let feature = |name: &str, axis: &str| json!({ "feature": name, "axis": axis, "option": "on", "description": "Adds one." });
    let with_features = |features: serde_json::Value| {
        json!({
            "options": [{ "option": "a", "description": "One.", "features": features }],
            "max_copies": 1,
        })
    };
    let refused = |value: serde_json::Value| scope(value).expect_err("the scope is refused");

    let none =
        scope(json!({ "options": [{ "option": "a", "description": "One." }], "max_copies": 1 }))
            .expect("an option with no features key offers none");
    assert!(none.options()[0].features().is_empty());

    let error = refused(with_features(json!([])));
    assert_eq!(
        error,
        "`features` of option `a` lists no feature; leave `features` out for an option with none"
    );

    let error = refused(with_features(json!([
        { "feature": "f", "axis": "x", "option": "on", "description": "One.", "with": {} }
    ])));
    assert!(error.contains("unknown field `with`"), "{error}");

    let error = refused(with_features(json!([feature("f", "x"), feature("f", "y")])));
    assert_eq!(
        error,
        "option `a` lists feature `f` twice, on axis `x` and on axis `y`; give each feature its \
         own name"
    );

    let error = refused(with_features(json!([feature("f", "x"), feature("g", "x")])));
    assert_eq!(
        error,
        "features `f` and `g` of option `a` both select axis `x`; a robot holds one option on an \
         axis, so an axis has at most one feature"
    );

    let six: Vec<serde_json::Value> = (0..MAX_FEATURES)
        .map(|index| feature(&format!("f{index}"), &format!("x{index}")))
        .collect();
    scope(with_features(json!(six))).expect("six features fit");
    let seven: Vec<serde_json::Value> = (0..=MAX_FEATURES)
        .map(|index| feature(&format!("f{index}"), &format!("x{index}")))
        .collect();
    let error = refused(with_features(json!(seven)));
    assert_eq!(
        error,
        "option `a` lists 7 features, and an option holds at most 6"
    );

    for (description, expected) in [
        (
            "  ",
            "the description of feature `f` of option `a` is empty",
        ),
        (
            "One.\nTwo.",
            "the description of feature `f` of option `a` is one line",
        ),
        (
            &*"é".repeat(MAX_DESCRIPTION_CHARS + 1),
            "the description of feature `f` of option `a` is at most 200 characters",
        ),
    ] {
        let error = refused(with_features(json!([
            { "feature": "f", "axis": "x", "option": "on", "description": description }
        ])));
        assert!(error.starts_with(expected), "{error}");
    }
}

/// The catalog of the design's `framework_controls:v1`, as the server
/// derives it before it narrows anything.
fn framework_controls_bundle() -> ExposureBundle {
    let exposure =
        PeppyMcpExposureParser::from_content(FRAMEWORK_CONTROLS).expect("the fixture parses");
    build_exposure_bundle(&exposure, &[], &[DaemonInterface::StackCopies.resolved()])
        .expect("the fixture validates against the interface")
        .bundle
}

fn task_schema<'a>(bundle: &'a ExposureBundle, tool: &str) -> &'a serde_json::Value {
    &bundle
        .tasks
        .iter()
        .find(|task| task.name == tool)
        .unwrap_or_else(|| panic!("no task `{tool}`"))
        .input_schema
}

/// The framework bundle with `value`'s scope narrowing its target.
fn narrowed_bundle(value: serde_json::Value) -> ExposureBundle {
    let mut bundle = framework_controls_bundle();
    DaemonInterface::StackCopies
        .parse_scope(&value)
        .expect("parses")
        .narrow(&mut bundle, "stack");
    bundle
}

#[test]
fn a_scope_narrows_the_published_schemas_of_its_target() {
    let derived = framework_controls_bundle();
    let feature_item = json!({
        "type": "object",
        "properties": {
            "feature": { "type": "string" },
            "core_node": { "type": "string" },
        },
        "required": ["feature", "core_node"],
        "additionalProperties": false,
    });
    assert_eq!(
        task_schema(&derived, "stack.join")["properties"],
        json!({
            "name": { "type": "string" },
            "option": { "type": "string" },
            "core_node": { "type": "string" },
            "features": { "type": "array", "items": feature_item },
        }),
        "the interface's own schema, which `peppy mcp catalog` prints"
    );
    let mut required: Vec<&str> = task_schema(&derived, "stack.join")["required"]
        .as_array()
        .expect("a required list")
        .iter()
        .map(|field| field.as_str().expect("a field name"))
        .collect();
    required.sort_unstable();
    assert_eq!(required, ["core_node", "features", "name", "option"]);

    let narrowed = narrowed_bundle(simulation_scope());
    let name_rule = config::runtime::CoreNodeName::json_schema();
    assert_eq!(
        task_schema(&narrowed, "stack.join")["properties"],
        json!({
            "name": name_rule,
            "option": { "type": "string", "enum": ["openarm_sim", "so101_sim"] },
            "core_node": name_rule,
            "features": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "feature": { "type": "string", "enum": ["ai_brain"] },
                        "core_node": name_rule,
                    },
                    "required": ["feature", "core_node"],
                    "additionalProperties": false,
                },
            },
        })
    );
    assert_eq!(
        task_schema(&narrowed, "stack.remove")["properties"],
        json!({ "name": name_rule })
    );
    assert_eq!(
        narrowed.tools, derived.tools,
        "`list` takes no input to narrow"
    );
    assert!(
        !task_schema(&narrowed, "stack.join")
            .to_string()
            .contains(r#""core_node":{"enum""#),
        "no machine list: the machines change while the endpoint runs"
    );

    // A scope with no feature takes an empty `features` only.
    let featureless = narrowed_bundle(featureless_scope());
    assert_eq!(
        task_schema(&featureless, "stack.join")["properties"]["features"],
        json!({ "type": "array", "maxItems": 0 })
    );

    // A scope narrows the entries of its own target alone.
    let mut elsewhere = derived.clone();
    DaemonInterface::StackCopies
        .parse_scope(&simulation_scope())
        .expect("parses")
        .narrow(&mut elsewhere, "other");
    assert_eq!(elsewhere, derived);
}

/// The narrowed input schemas judge input as the MCP server runtime does:
/// it compiles each input schema with `jsonschema::validator_for` and
/// refuses a call whose arguments do not validate.
#[test]
fn the_runtime_validator_refuses_input_outside_the_narrowed_schema() {
    let bundle = narrowed_bundle(simulation_scope());
    let join = jsonschema::validator_for(task_schema(&bundle, "stack.join"))
        .expect("the narrowed join schema compiles");
    let remove = jsonschema::validator_for(task_schema(&bundle, "stack.remove"))
        .expect("the narrowed remove schema compiles");
    let goal = |name: serde_json::Value,
                core_node: serde_json::Value,
                features: serde_json::Value| {
        json!({ "name": name, "option": "openarm_sim", "core_node": core_node, "features": features })
    };

    assert!(join.is_valid(&goal(json!("bravo"), json!("cn-desk"), json!([]))));
    assert!(join.is_valid(&goal(
        json!("Bravo_2-x"),
        json!("cn-desk"),
        json!([{ "feature": "ai_brain", "core_node": "cn-gpu-box" }])
    )));
    assert!(join.is_valid(&goal(
        json!("n".repeat(63)),
        json!("n".repeat(63)),
        json!([])
    )));
    assert!(remove.is_valid(&json!({ "name": "bravo" })));

    for name in [
        json!("self"),
        json!("bad name"),
        json!("n".repeat(64)),
        json!(""),
        json!("a/b"),
        json!("a@b"),
        json!(7),
    ] {
        assert!(
            !join.is_valid(&goal(name.clone(), json!("cn-desk"), json!([]))),
            "join takes the name {name}"
        );
        assert!(
            !join.is_valid(&goal(json!("bravo"), name.clone(), json!([]))),
            "join takes the machine {name}"
        );
        assert!(
            !join.is_valid(&goal(
                json!("bravo"),
                json!("cn-desk"),
                json!([{ "feature": "ai_brain", "core_node": name }])
            )),
            "a feature takes the machine {name}"
        );
        assert!(
            !remove.is_valid(&json!({ "name": name })),
            "remove takes the name {name}"
        );
    }
    assert!(
        !join.is_valid(&json!({ "name": "bravo", "option": "openarm_real", "core_node": "cn-desk", "features": [] })),
        "an option outside the scope"
    );
    assert!(
        !join.is_valid(&goal(
            json!("bravo"),
            json!("cn-desk"),
            json!([{ "feature": "camera", "core_node": "cn-desk" }])
        )),
        "a feature outside the scope"
    );
    assert!(
        !join.is_valid(&goal(
            json!("bravo"),
            json!("cn-desk"),
            json!([{ "feature": "ai_brain" }])
        )),
        "a feature item with no machine"
    );
    assert!(
        !join.is_valid(&goal(
            json!("bravo"),
            json!("cn-desk"),
            json!([{ "feature": "ai_brain", "core_node": "cn-desk", "axis": "brain" }])
        )),
        "a feature item with a field outside the goal"
    );
    for missing in ["name", "option", "core_node", "features"] {
        let mut partial = goal(json!("bravo"), json!("cn-desk"), json!([]));
        partial.as_object_mut().expect("an object").remove(missing);
        assert!(!join.is_valid(&partial), "{missing} is required");
    }
    let mut extra = goal(json!("bravo"), json!("cn-desk"), json!([]));
    extra["with"] = json!("x");
    assert!(
        !join.is_valid(&extra),
        "no field outside the interface's goal"
    );

    // A listed copy has the shape of a join goal.
    let listed = json!({
        "name": "delta",
        "option": "openarm_sim",
        "core_node": "cn-jetson-1",
        "features": [{ "feature": "ai_brain", "core_node": "cn-gpu-box" }],
    });
    assert!(join.is_valid(&listed), "a listed copy is a valid join goal");

    let featureless = narrowed_bundle(featureless_scope());
    let featureless_join = jsonschema::validator_for(task_schema(&featureless, "stack.join"))
        .expect("the narrowed join schema compiles");
    assert!(featureless_join.is_valid(&goal(json!("bravo"), json!("cn-desk"), json!([]))));
    assert!(!featureless_join.is_valid(&goal(
        json!("bravo"),
        json!("cn-desk"),
        json!([{ "feature": "ai_brain", "core_node": "cn-desk" }])
    )));

    // The pattern and the rule agree on every name the daemon and the CLI
    // judge.
    let names = [
        "a", "self", "Self", "selfish", "a b", "a.b", "a/b", "é", "_", "-", "n", "x_y-Z9",
    ];
    for name in
        names
            .into_iter()
            .map(str::to_owned)
            .chain(["n".repeat(63), "n".repeat(64), String::new()])
    {
        assert_eq!(
            remove.is_valid(&json!({ "name": name })),
            config::runtime::CoreNodeName::new(name.as_str()).is_ok(),
            "{name:?}"
        );
    }
}
