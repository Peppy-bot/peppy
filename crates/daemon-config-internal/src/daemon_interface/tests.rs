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
            { "option": "openarm_sim", "description": "A simulated OpenArm v2 standing on the floor" },
            { "option": "so101_sim", "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

#[test]
fn a_stack_copies_scope_parses_into_its_type() {
    let parsed = scope(simulation_scope()).expect("parses");
    let options: Vec<(&str, &str)> = parsed
        .options()
        .iter()
        .map(|entry| (entry.option.as_str(), entry.description.as_str()))
        .collect();
    assert_eq!(
        options,
        [
            (
                "openarm_sim",
                "A simulated OpenArm v2 standing on the floor"
            ),
            (
                "so101_sim",
                "A simulated SO-101 clamped on the edge of a table"
            ),
        ]
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
    assert!(
        error.starts_with("options[0].description:") && error.contains("empty"),
        "{error}"
    );
    let error = refused(json!({ "options": [option("a", "One.\nTwo.")], "max_copies": 1 }));
    assert!(error.contains("one line"), "{error}");
    let long = "é".repeat(MAX_DESCRIPTION_CHARS + 1);
    let error = refused(json!({ "options": [option("a", &long)], "max_copies": 1 }));
    assert!(
        error.contains("at most 200 characters, and this one has 201"),
        "{error}"
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

#[test]
fn a_scope_narrows_the_published_schemas_of_its_target() {
    let derived = framework_controls_bundle();
    assert_eq!(
        task_schema(&derived, "stack.join")["properties"],
        json!({ "name": { "type": "string" }, "option": { "type": "string" } }),
        "the interface's own schema, which `peppy mcp catalog` prints"
    );

    let mut narrowed = derived.clone();
    DaemonInterface::StackCopies
        .parse_scope(&simulation_scope())
        .expect("parses")
        .narrow(&mut narrowed, "stack");
    let copy_name = config::runtime::CoreNodeName::json_schema();
    assert_eq!(
        task_schema(&narrowed, "stack.join")["properties"],
        json!({
            "name": copy_name,
            "option": { "type": "string", "enum": ["openarm_sim", "so101_sim"] },
        })
    );
    assert_eq!(
        task_schema(&narrowed, "stack.remove")["properties"],
        json!({ "name": copy_name })
    );
    assert_eq!(
        narrowed.tools, derived.tools,
        "`list` takes no input to narrow"
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
    let mut bundle = framework_controls_bundle();
    DaemonInterface::StackCopies
        .parse_scope(&simulation_scope())
        .expect("parses")
        .narrow(&mut bundle, "stack");
    let join = jsonschema::validator_for(task_schema(&bundle, "stack.join"))
        .expect("the narrowed join schema compiles");
    let remove = jsonschema::validator_for(task_schema(&bundle, "stack.remove"))
        .expect("the narrowed remove schema compiles");

    assert!(join.is_valid(&json!({ "name": "bravo", "option": "so101_sim" })));
    assert!(join.is_valid(&json!({ "name": "Bravo_2-x", "option": "openarm_sim" })));
    assert!(join.is_valid(&json!({ "name": "n".repeat(63), "option": "openarm_sim" })));
    assert!(remove.is_valid(&json!({ "name": "bravo" })));

    for name in [
        json!("self"),
        json!("bad name"),
        json!("n".repeat(64)),
        json!(""),
        json!("a/b"),
        json!(7),
    ] {
        assert!(
            !join.is_valid(&json!({ "name": name, "option": "so101_sim" })),
            "join takes the name {name}"
        );
        assert!(
            !remove.is_valid(&json!({ "name": name })),
            "remove takes the name {name}"
        );
    }
    assert!(
        !join.is_valid(&json!({ "name": "bravo", "option": "openarm_real" })),
        "an option outside the scope"
    );
    assert!(
        !join.is_valid(&json!({ "name": "bravo" })),
        "the option is required"
    );
    assert!(
        !join.is_valid(&json!({ "name": "bravo", "option": "so101_sim", "with": "x" })),
        "no field outside the interface's goal"
    );

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
