//! `env_from_launch` and `set_env_from_launch`: the names a launcher gives
//! the variables that an instance takes from the launch environment, their
//! parse and composition checks, and what the launch checks of daemon scopes
//! report of the instances that a join can start.

use daemon_config::launcher::{
    AppliedChange, CompositionError, CopyMembership, Deployment, DeploymentInstance,
    DeploymentSource, DeploymentTargets, JoinRequest, Joinable, LaunchVariable, PeppyLauncher,
    PeppyLauncherParser, PreparedLauncher, RunningStack, ScopeCheck, Starter, VariableChange,
    VariableNeed, check_daemon_scopes,
};
use std::path::Path;

mod common;
use common::{copy_name, exposure_targets, fragment_file, join_words, name, words, write};

/// The brain of the example arm, as the launcher of the work item writes it.
const BRAIN: &str = r#"{
    instance_id: "brain_inst",
    arguments: { model: "gemini-robotics-er-2-preview" },
    links: { limb_motion: "backbone_inst" },
    env_from_launch: ["GEMINI_API_KEY"],
}"#;

/// A launcher in the shape of the example: the framework's endpoint, whose
/// scope offers `demo_arm`, and a copy axis whose `demo_arm` option holds a
/// backbone and a brain. Each field adds to or replaces a part of it.
struct Demo<'a> {
    /// The brain instance of `demo_arm`.
    brain: &'a str,
    /// The own axes of `demo_arm`.
    own_axes: &'a str,
    /// The features of `demo_arm` in the scope.
    features: &'a str,
    /// The adjustments of the launcher under `demo_arm`.
    option_adjustments: &'a str,
    /// The launcher's adjustments after the one that scopes the endpoint.
    adjustments: &'a str,
}

impl Default for Demo<'_> {
    fn default() -> Self {
        Self {
            brain: BRAIN,
            own_axes: "",
            features: "",
            option_adjustments: "",
            adjustments: "",
        }
    }
}

impl Demo<'_> {
    fn document(&self) -> String {
        let Self {
            brain,
            own_axes,
            features,
            option_adjustments,
            adjustments,
        } = self;
        let features = if features.is_empty() {
            String::new()
        } else {
            format!("features: [{features}],")
        };
        let option_adjustments = if option_adjustments.is_empty() {
            String::new()
        } else {
            format!("adjustments: [{option_adjustments}],")
        };
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            description: ["A simulated arm whose brain asks a hosted model, added over MCP."],
            components: [
                {{ name: "simulation", cardinality: "one", options: {{
                    waldo: {{ deployments: [
                        {{ source: {{ name: "waldo", tag: "v1" }}, instances: [{{ instance_id: "simulation_inst" }}] }},
                    ] }},
                }} }},
                {{ name: "robot_control", cardinality: "one", options: {{
                    robot_control: {{ deployments: [
                        {{ source: {{ exposures: ["framework_controls:v1"] }}, instances: [
                            {{ instance_id: "framework_controls_inst" }},
                        ] }},
                    ] }},
                    none: {{}},
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    demo_arm: {{
                        deployments: [
                            {{ source: {{ name: "demo_arm_backbone", tag: "v1" }}, instances: [
                                {{ instance_id: "backbone_inst", links: {{ arm: "simulation_inst/arms" }} }},
                            ] }},
                            {{ source: {{ name: "demo_hosted_brain", tag: "v1" }}, instances: [{brain}] }},
                        ],
                        components: [{own_axes}],
                        {option_adjustments}
                    }},
                }} }},
            ],
            deployments: [
                {{ simulation: "waldo" }},
                {{ robot_control: "robot_control" }},
                {{ robot: "demo_arm" }},
            ],
            adjustments: [
                {{ target: "framework_controls_inst", when: {{ robot_control: "robot_control" }},
                  set_daemon_scopes: {{ stack: {{
                    max_copies: 2,
                    options: [{{ option: "demo_arm", model: "demo_arm",
                                 description: "A simulated arm with a brain on a hosted model",
                                 {features} }}],
                  }} }} }},
                {adjustments}
            ],
        }}"#
        )
    }
}

fn demo_launcher() -> String {
    Demo::default().document()
}

/// An own axis `model` of `demo_arm` whose option `hosted` brings a proxy
/// instance, and the scope's feature `hosted_model` that selects it.
const MODEL_AXIS: &str = r#"{ name: "model", cardinality: "zero_or_one", options: {
    hosted: { deployments: [
        { source: { name: "model_proxy", tag: "v1" }, instances: [{ instance_id: "proxy_inst" }] },
    ] },
} }"#;
const HOSTED_MODEL: &str = r#"{ feature: "hosted_model", axis: "model", option: "hosted",
    description: "Adds a hosted model." }"#;
/// The adjustment of `demo_arm` that gives the brain the key of the hosted
/// model, when the copy selects it.
const HOSTED_KEY: &str = r#"{ target: "brain_inst", when: { model: "hosted" },
    set_env_from_launch: ["GEMINI_API_KEY"] }"#;

fn load(document: &str) -> PreparedLauncher {
    let parsed = PeppyLauncherParser::from_content(document).expect("the launcher parses");
    PreparedLauncher::load(&parsed, Path::new("demo.json5")).expect("it loads")
}

fn instance<'a>(flat: &'a PeppyLauncher, id: &str) -> &'a DeploymentInstance {
    flat.deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .find(|instance| instance.instance_id.as_str() == id)
        .unwrap_or_else(|| panic!("{id} is in the plan"))
}

fn names(instance: &DeploymentInstance) -> Vec<&str> {
    instance
        .env_from_launch
        .iter()
        .map(LaunchVariable::as_str)
        .collect()
}

fn targets_of(deployment: &Deployment) -> DeploymentTargets {
    match &deployment.source {
        DeploymentSource::Exposures { exposures } => exposure_targets(exposures),
        DeploymentSource::Node { .. } => DeploymentTargets::default(),
    }
}

/// What the joins of a launch of `prepared` with `launch_words` can start,
/// as the launch checks of daemon scopes report it.
fn joinable(prepared: &PreparedLauncher, launch_words: &[&str]) -> Result<Joinable, String> {
    let composed = prepared
        .launch(&words(launch_words), &[])
        .map_err(|error| error.to_string())?;
    let targets: Vec<DeploymentTargets> = composed
        .launcher
        .deployments
        .iter()
        .map(targets_of)
        .collect();
    check_daemon_scopes(
        prepared,
        composed.launcher.deployments.iter().zip(&targets),
        &CopyMembership::of(composed.copies()),
        ScopeCheck::Launch(&composed.selection),
    )
    .map_err(|refusals| refusals.to_string())
}

fn scoped_need(variable: &str, instance: &str, features: &[&str]) -> VariableNeed {
    VariableNeed {
        variable: LaunchVariable::try_from(variable.to_owned()).unwrap(),
        instance: instance.to_owned(),
        starter: Starter::ScopedJoin {
            option: name("demo_arm"),
            features: features.iter().map(|feature| name(feature)).collect(),
            scopes: vec!["framework_controls_inst".to_owned()],
        },
    }
}

// -- what a join can start -----------------------------------------------

/// The launch of the example starts no arm, and a join through the
/// framework's endpoint can add one whose brain takes the key: the launch
/// needs the key, and the refusal names the variable, the instance, the
/// option and the instance whose scope offers it.
#[test]
fn the_example_needs_the_key_of_the_brain_that_a_join_can_add() {
    let prepared = load(&demo_launcher());
    let joinable = joinable(&prepared, &[]).expect("the scopes hold");
    assert_eq!(
        joinable.variable_needs.iter().cloned().collect::<Vec<_>>(),
        [scoped_need("GEMINI_API_KEY", "brain_inst", &[])]
    );
    let refusal = joinable
        .variable_needs
        .check(VariableChange::Launch, |_| false)
        .expect_err("an empty launch environment holds no key")
        .to_string();
    assert_eq!(
        refusal,
        "the launch needs `GEMINI_API_KEY`, and the launch environment does not hold it, or \
         holds a value with characters that a node cannot receive: `brain_inst` of option \
         `demo_arm` takes it from the launch, and stack.join through `framework_controls_inst` \
         can add a copy of `demo_arm`. Export `GEMINI_API_KEY` in the shell that launches, with \
         a value of letters, digits and `_ - . / : , = @ + %` only"
    );
}

/// With `robot_control=none` no endpoint serves a scope, so no join can add
/// an arm and the launch needs no key.
#[test]
fn without_the_endpoint_no_join_needs_a_key() {
    let prepared = load(&demo_launcher());
    let joinable = joinable(&prepared, &["robot_control=none"]).expect("the scopes hold");
    assert!(joinable.variable_needs.is_empty());
}

/// A brain that takes the key only under a feature of the scope, through a
/// `set_env_from_launch` under the `when` of that feature's option, is a
/// need of the copy with that feature, and the refusal names the feature.
#[test]
fn a_need_under_a_feature_names_the_feature() {
    let prepared = load(
        &Demo {
            brain: r#"{ instance_id: "brain_inst" }"#,
            own_axes: MODEL_AXIS,
            features: HOSTED_MODEL,
            option_adjustments: HOSTED_KEY,
            ..Demo::default()
        }
        .document(),
    );
    let joinable = joinable(&prepared, &[]).expect("the scopes hold");
    assert_eq!(
        joinable.variable_needs.iter().cloned().collect::<Vec<_>>(),
        [scoped_need(
            "GEMINI_API_KEY",
            "brain_inst",
            &["hosted_model"]
        )]
    );
    let refusal = joinable
        .variable_needs
        .check(VariableChange::Launch, |_| false)
        .expect_err("no key")
        .to_string();
    assert!(
        refusal.contains("can add a copy of `demo_arm` with the feature `hosted_model`"),
        "{refusal}"
    );
}

/// An instance that only a word of the CLI selects, an option of an own
/// axis that no feature of the scope offers, makes no launch need its
/// variable.
#[test]
fn an_instance_that_only_a_cli_word_selects_makes_no_launch_need_its_variable() {
    let prepared = load(
        &Demo {
            brain: r#"{ instance_id: "brain_inst" }"#,
            own_axes: MODEL_AXIS,
            option_adjustments: HOSTED_KEY,
            ..Demo::default()
        }
        .document(),
    );
    let joinable = joinable(&prepared, &[]).expect("the scopes hold");
    assert!(joinable.variable_needs.is_empty());

    // A join with the word starts the brain with the variable.
    let composed = prepared.launch(&[], &[]).expect("composes");
    let joined = prepared
        .join(
            JoinRequest {
                option: &name("demo_arm"),
                name: &copy_name("alpha"),
                words: &join_words(&["model=hosted"]),
                arguments: &[],
            },
            RunningStack {
                selection: &composed.selection,
                launcher: &composed.launcher,
            },
        )
        .expect("the join composes");
    assert_eq!(
        names(instance(&joined.launcher, "alpha_brain_inst")),
        ["GEMINI_API_KEY"]
    );
}

/// A copy the launch starts carries its brain's list in the flat document,
/// which `peppy stack resolve` prints with the names only.
#[test]
fn a_copy_the_launch_starts_carries_the_names_in_the_flat_document() {
    let prepared = load(&demo_launcher());
    let composed = prepared
        .launch(&[], &["demo_arm:alpha".parse().unwrap()])
        .expect("composes");
    let brain = instance(&composed.launcher, "alpha_brain_inst");
    assert_eq!(names(brain), ["GEMINI_API_KEY"]);
    let printed = serde_json5::to_string(&composed.launcher).expect("serializes");
    assert!(
        printed.contains(r#""env_from_launch":["GEMINI_API_KEY"]"#),
        "{printed}"
    );
    let reparsed = PeppyLauncherParser::from_content(&printed).expect("the flat file parses");
    assert_eq!(
        names(instance(&reparsed, "alpha_brain_inst")),
        ["GEMINI_API_KEY"]
    );
}

// -- set_env_from_launch -------------------------------------------------

/// `set_env_from_launch` replaces the list of its target under its `when`,
/// and the report gives the list before and after.
#[test]
fn set_env_from_launch_replaces_the_list_under_its_guard() {
    let prepared = load(
        &Demo {
            adjustments: r#"{ target: "brain_inst", when: { simulation: "waldo" },
                set_env_from_launch: ["OPENROUTER_API_KEY", "GEMINI_API_KEY"] },"#,
            ..Demo::default()
        }
        .document(),
    );
    let composed = prepared
        .launch(&[], &["demo_arm:alpha".parse().unwrap()])
        .expect("composes");
    assert_eq!(
        names(instance(&composed.launcher, "alpha_brain_inst")),
        ["OPENROUTER_API_KEY", "GEMINI_API_KEY"]
    );
    let written: Vec<(&str, &AppliedChange)> = composed
        .report
        .applied
        .iter()
        .filter(|entry| matches!(entry.change, AppliedChange::EnvFromLaunch { .. }))
        .map(|entry| (entry.target.as_str(), &entry.change))
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0].0, "alpha_brain_inst");
    let rendered = composed.report.render_lines().join("\n");
    assert!(
        rendered.contains(
            r#"alpha_brain_inst.env_from_launch: ["GEMINI_API_KEY"] -> ["OPENROUTER_API_KEY","GEMINI_API_KEY"]"#
        ),
        "{rendered}"
    );

    let joinable = joinable(&prepared, &[]).expect("the scopes hold");
    assert_eq!(
        joinable.variable_needs.iter().cloned().collect::<Vec<_>>(),
        [
            scoped_need("OPENROUTER_API_KEY", "brain_inst", &[]),
            scoped_need("GEMINI_API_KEY", "brain_inst", &[]),
        ]
    );
}

/// An empty `set_env_from_launch` is an operation: it clears the list, so
/// the launch needs no key and a joined brain takes none.
#[test]
fn an_empty_set_env_from_launch_clears_the_list() {
    let prepared = load(
        &Demo {
            adjustments: r#"{ target: "brain_inst", set_env_from_launch: [] },"#,
            ..Demo::default()
        }
        .document(),
    );
    let joinable = joinable(&prepared, &[]).expect("the scopes hold");
    assert!(joinable.variable_needs.is_empty());
    let composed = prepared
        .launch(&[], &["demo_arm:alpha".parse().unwrap()])
        .expect("composes");
    assert!(
        instance(&composed.launcher, "alpha_brain_inst")
            .env_from_launch
            .is_empty()
    );
}

/// The names a launch keeps the values of: every name of the launcher and of
/// the fragments of its options, selected or not, in an instance or in an
/// adjustment.
#[test]
fn the_kept_names_are_every_name_of_the_launcher_and_its_fragments() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("fragments/brain.json5"),
        &fragment_file(
            r#"deployments: [
                { source: { name: "brain", tag: "v1" }, instances: [
                    { instance_id: "fragment_brain_inst", env_from_launch: ["FRAGMENT_KEY"] } ] } ],
               adjustments: [{ target: "fragment_brain_inst", set_env_from_launch: ["FRAGMENT_ADJUSTED_KEY"] }]"#,
        ),
    );
    let launcher = write(
        &dir.path().join("launcher.json5"),
        r#"{
            peppy_schema: "launcher/v1",
            description: ["A test launcher."],
            components: [
                { name: "brain", cardinality: "one", options: {
                    hosted: "fragments/brain.json5",
                    inline: {
                        deployments: [{ source: { name: "brain", tag: "v1" }, instances: [
                            { instance_id: "inline_brain_inst", env_from_launch: ["INLINE_KEY"] } ] }],
                        adjustments: [{ target: "inline_brain_inst", set_env_from_launch: ["OPTION_KEY"] }],
                    },
                } },
            ],
            deployments: [
                { source: { name: "planner", tag: "v1" }, instances: [
                    { instance_id: "planner_inst", env_from_launch: ["PLANNER_KEY"] } ] },
                { brain: "hosted" },
            ],
            adjustments: [{ target: "planner_inst", when: { brain: "inline" }, set_env_from_launch: ["TOP_KEY"] }],
        }"#,
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).unwrap();
    let prepared = PreparedLauncher::load(&parsed, &launcher).unwrap();
    assert_eq!(
        prepared
            .launch_variable_names()
            .iter()
            .map(LaunchVariable::as_str)
            .collect::<Vec<_>>(),
        [
            "FRAGMENT_ADJUSTED_KEY",
            "FRAGMENT_KEY",
            "INLINE_KEY",
            "OPTION_KEY",
            "PLANNER_KEY",
            "TOP_KEY"
        ]
    );
}

// -- a variable given two values -----------------------------------------

/// An instance that takes a variable from the launch and gives it a value
/// in `env_vars` after the adjustments is refused at the launch, naming the
/// instance, the variable and the adjustment that set the list.
#[test]
fn a_variable_in_env_vars_and_the_list_is_refused_at_the_launch() {
    let prepared = load(
        &Demo {
            adjustments: r#"{ target: "simulation_inst", set_env_from_launch: ["SIM_KEY"] },"#,
            ..Demo::default()
        }
        .document()
        .replace(
            r#"{ instance_id: "simulation_inst" }"#,
            r#"{ instance_id: "simulation_inst", env_vars: { SIM_KEY: "written" } }"#,
        ),
    );
    let refusal = prepared.launch(&[], &[]).expect_err("two values");
    assert!(
        matches!(&refusal, CompositionError::VariableGivenTwoValues { .. }),
        "{refusal:?}"
    );
    assert_eq!(
        refusal.to_string(),
        "instance `simulation_inst` takes `SIM_KEY` from the launch in `env_from_launch` (set by \
         the adjustment in demo.json5, top-level `adjustments`) and gives it a value in \
         `env_vars`, so the variable would get two values; take it out of one of the two"
    );
}

/// The same refusal reaches a trial copy of a feature: the launch checks of
/// the scope name the feature set.
#[test]
fn a_variable_given_two_values_in_a_trial_copy_names_its_feature() {
    let prepared = load(
        &Demo {
            brain: r#"{ instance_id: "brain_inst", env_vars: { GEMINI_API_KEY: "written" } }"#,
            own_axes: MODEL_AXIS,
            features: HOSTED_MODEL,
            option_adjustments: HOSTED_KEY,
            ..Demo::default()
        }
        .document(),
    );
    let refusal = joinable(&prepared, &[]).expect_err("the trial copy is refused");
    assert!(
        refusal.contains(
            "option `demo_arm` with the features `hosted_model`: instance `brain_inst` takes \
             `GEMINI_API_KEY` from the launch"
        ),
        "{refusal}"
    );
}

/// A join that composes a copy whose instance would get two values is
/// refused, and so is a flat file that holds one, as `peppy stack resolve`
/// prints it.
#[test]
fn a_variable_given_two_values_is_refused_at_a_join_and_in_a_flat_file() {
    let prepared = load(
        &Demo {
            brain: r#"{ instance_id: "brain_inst", env_vars: { GEMINI_API_KEY: "written" } }"#,
            own_axes: MODEL_AXIS,
            option_adjustments: HOSTED_KEY,
            ..Demo::default()
        }
        .document(),
    );
    let composed = prepared.launch(&[], &[]).expect("composes");
    let refusal = prepared
        .join(
            JoinRequest {
                option: &name("demo_arm"),
                name: &copy_name("alpha"),
                words: &join_words(&["model=hosted"]),
                arguments: &[],
            },
            RunningStack {
                selection: &composed.selection,
                launcher: &composed.launcher,
            },
        )
        .expect_err("two values");
    assert!(
        matches!(refusal, CompositionError::VariableGivenTwoValues { .. }),
        "{refusal}"
    );

    let refusal = PeppyLauncherParser::from_content(
        r#"{
            peppy_schema: "launcher/v1",
            description: ["A flat launcher."],
            deployments: [{ source: { name: "brain", tag: "v1" }, instances: [
                { instance_id: "brain_inst", env_vars: { GEMINI_API_KEY: "written" },
                  env_from_launch: ["GEMINI_API_KEY"] } ] }],
        }"#,
    )
    .expect_err("two values")
    .to_string();
    assert!(
        refusal.contains(
            "instance `brain_inst` takes `GEMINI_API_KEY` from the launch in `env_from_launch` \
             and gives it a value in `env_vars`"
        ),
        "{refusal}"
    );
}

// -- parse errors --------------------------------------------------------

/// A flat launcher whose one instance writes `env_from_launch: list`.
fn flat_with(list: &str) -> String {
    format!(
        r#"{{
            peppy_schema: "launcher/v1",
            description: ["A flat launcher."],
            deployments: [{{ source: {{ name: "brain", tag: "v1" }}, instances: [
                {{ instance_id: "brain_inst", env_from_launch: {list} }} ] }}],
        }}"#
    )
}

/// Each name of a list is a shell identifier, no forbidden key, no name the
/// CLI never forwards, and named once; each refusal names the instance and
/// the variable.
#[test]
fn a_list_refuses_a_name_the_launch_environment_cannot_give() {
    let mut cases = vec![
        (
            r#"["has-dash"]"#.to_owned(),
            "`has-dash` is not a valid shell identifier".to_owned(),
        ),
        (
            r#"["LD_PRELOAD"]"#.to_owned(),
            "`LD_PRELOAD` is reserved".to_owned(),
        ),
        (
            r#"["GEMINI_API_KEY", "GEMINI_API_KEY"]"#.to_owned(),
            "names `GEMINI_API_KEY` two times".to_owned(),
        ),
    ];
    for never in daemon_config::env::NEVER_FORWARDED_ENV_KEYS {
        cases.push((
            format!(r#"["{never}"]"#),
            format!("`{never}` is never forwarded by the CLI"),
        ));
    }
    for (list, expected) in cases {
        let refusal = PeppyLauncherParser::from_content(&flat_with(&list))
            .expect_err(&list)
            .to_string();
        assert!(
            refusal.contains("instance `brain_inst`: ") && refusal.contains(&expected),
            "{list}: {refusal}"
        );
    }
    PeppyLauncherParser::from_content(&flat_with("[]")).expect("an empty list is no list");
}

/// An adjustment's list is held to the same rules, and the refusal names
/// where the adjustment is written.
#[test]
fn set_env_from_launch_refuses_a_name_the_launch_environment_cannot_give() {
    for (list, expected) in [
        (r#"["PWD"]"#, "`PWD` is never forwarded by the CLI"),
        (r#"["A", "A"]"#, "names `A` two times"),
    ] {
        let adjustment = format!(r#"{{ target: "brain_inst", set_env_from_launch: {list} }},"#);
        let document = Demo {
            adjustments: &adjustment,
            ..Demo::default()
        }
        .document();
        let refusal = PeppyLauncherParser::from_content(&document)
            .expect_err(list)
            .to_string();
        assert!(
            refusal.contains("adjustments[1].set_env_from_launch") && refusal.contains(expected),
            "{list}: {refusal}"
        );
    }
}
