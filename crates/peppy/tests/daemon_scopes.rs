//! `peppy stack resolve` holds a launcher to the launch checks of daemon
//! scopes with a cold nodes cache: the checks read the launcher and the
//! exposure documents alone, so a machine that holds no node, and no
//! contract, still refuses every scope a launch would refuse.

use daemon_config::consts::PeppyDirs;
use peppy::commands::stack::resolve_rendered;
use std::fs;
use std::path::{Path, PathBuf};

/// The design's `framework/framework_controls.json5`, kept beside the
/// registry of the daemon interfaces whose `robot_membership:v1` it names.
const FRAMEWORK_CONTROLS: &str = include_str!(
    "../../daemon-config-internal/src/daemon_interface/fixtures/framework_controls.json5"
);

/// The scope `simulation_mcp` gives its framework endpoint.
const SCOPE: &str = r#"{
    max_copies: 4,
    options: [
        { option: "openarm_sim", model: "openarm_v2", description: "A simulated OpenArm v2 standing on the floor" },
        { option: "so101_sim", model: "so101", description: "A simulated SO-101 clamped on the edge of a table" },
    ],
}"#;

/// A peppy home whose exposure cache holds `framework_controls:v1`,
/// `unserved_controls:v1`, the same document naming a tag of `robot_membership`
/// this peppy does not serve, and `pinned_controls:v1`, the same document
/// pinning its daemon target with a `sha256`, which does not parse. Its nodes
/// and contracts caches are empty, and it holds no `absent_controls:v1`.
fn cold_home() -> (PeppyDirs, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("temp peppy root");
    let docs = root.path().join("exposures");
    fs::create_dir_all(&docs).expect("exposure dir");
    let framework = docs.join("framework_controls.json5");
    fs::write(&framework, FRAMEWORK_CONTROLS).expect("exposure");
    let unserved = docs.join("unserved_controls.json5");
    fs::write(
        &unserved,
        FRAMEWORK_CONTROLS
            .replace(
                r#"name: "framework_controls""#,
                r#"name: "unserved_controls""#,
            )
            .replace(
                r#"daemon: { name: "robot_membership", tag: "v1" }"#,
                r#"daemon: { name: "robot_membership", tag: "v2" }"#,
            ),
    )
    .expect("exposure");
    let pinned = docs.join("pinned_controls.json5");
    fs::write(
        &pinned,
        FRAMEWORK_CONTROLS
            .replace(
                r#"name: "framework_controls""#,
                r#"name: "pinned_controls""#,
            )
            .replace(
                r#"daemon: { name: "robot_membership", tag: "v1" }"#,
                &format!(
                    r#"daemon: {{ name: "robot_membership", tag: "v1", sha256: "{}" }}"#,
                    "0".repeat(64)
                ),
            ),
    )
    .expect("exposure");
    let dirs = PeppyDirs::new(root.path());
    fs::create_dir_all(dirs.cache_dir()).expect("cache dir");
    core_node::test_support::seed_exposure_cache(
        &dirs,
        &[
            ("framework_controls", "v1", framework.as_path()),
            ("unserved_controls", "v1", unserved.as_path()),
            ("pinned_controls", "v1", pinned.as_path()),
        ],
    );
    (dirs, root)
}

/// The shape of `simulation_mcp`: the framework's endpoint under the
/// `robot_control` option, the robots on an axis that runs as copies, and
/// the top-level adjustments `adjustments`. `endpoint` is the instance
/// entry of the endpoint.
fn launcher(root: &Path, endpoint: &str, adjustments: &str) -> PathBuf {
    launcher_with(root, endpoint, adjustments, "")
}

/// [`launcher`] with the top-level deployments `extra_deployments` beside
/// the endpoint's option.
fn launcher_with(
    root: &Path,
    endpoint: &str,
    adjustments: &str,
    extra_deployments: &str,
) -> PathBuf {
    let path = root.join("simulation_mcp.json5");
    fs::write(
        &path,
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            description: ["A test launcher."],
            core_nodes: ["robot_pc"],
            components: [
                {{ name: "robot_control", cardinality: "one", options: {{
                    robot_control: {{ deployments: [
                        {{ source: {{ exposures: ["framework_controls:v1"] }}, instances: [
                            {endpoint}
                        ] }}
                    ] }},
                    none: {{}},
                }} }},
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    openarm_sim: {{ deployments: [
                        {{ source: {{ name: "openarm", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst" }}
                        ] }}
                    ] }},
                    so101_sim: {{ deployments: [
                        {{ source: {{ name: "so101", tag: "v1" }}, instances: [
                            {{ instance_id: "arm_inst" }}
                        ] }}
                    ] }},
                }} }},
            ],
            deployments: [{{ robot_control: "robot_control" }}, {extra_deployments}],
            adjustments: [{adjustments}],
        }}"#
        ),
    )
    .expect("launcher");
    path
}

const ENDPOINT: &str = r#"{ instance_id: "framework_controls_inst", arguments: { port: 8903 } }"#;

/// The scope of the `fleet` target of `framework_controls:v1`, as the
/// framework endpoints of the launchers give it.
const FLEET_SCOPE: &str = r#"{ members: ["list", "halt", "clear_halt"] }"#;

fn scoped(scope: &str) -> String {
    format!(
        r#"{{ target: "framework_controls_inst", set_daemon_scopes: {{ stack: {scope}, fleet: {FLEET_SCOPE} }} }}"#
    )
}

fn refusal(dirs: &PeppyDirs, launcher: PathBuf) -> String {
    resolve_rendered(dirs, launcher, &[], &[])
        .expect_err("the launcher is refused")
        .to_string()
}

#[test]
fn a_scoped_daemon_target_resolves_with_a_cold_nodes_cache() {
    let (dirs, root) = cold_home();
    let (document, report) = resolve_rendered(
        &dirs,
        launcher(root.path(), ENDPOINT, &scoped(SCOPE)),
        &[],
        &[],
    )
    .expect("the scope holds");
    let report = report.join("\n");
    assert!(
        report.contains(
            "daemon scopes hold: 2 daemon target(s) scoped and placed on the coordinator"
        ),
        "{report}"
    );
    assert!(
        report.contains("link rules not checked: the nodes cache is empty"),
        "{report}"
    );
    assert!(
        document.contains("daemon_scopes") && document.contains("so101_sim"),
        "the flat launcher carries the scope: {document}"
    );
}

/// The flat launcher `peppy stack resolve` prints keeps the scope and drops
/// the axes, so no option of a `robot_membership` scope runs as copies there:
/// the flat file is refused, as its launch refuses it, and the refusal says
/// why and what to launch instead.
#[test]
fn the_flat_launcher_of_a_scoped_launcher_is_refused_and_says_why() {
    let (dirs, root) = cold_home();
    let (document, _report) = resolve_rendered(
        &dirs,
        launcher(root.path(), ENDPOINT, &scoped(SCOPE)),
        &[],
        &[],
    )
    .expect("the scope holds");
    let flat = root.path().join("simulation_mcp_flat.json5");
    fs::write(&flat, document).expect("the flat launcher");
    let error = refusal(&dirs, flat);
    assert!(
        error.contains("this launcher declares no axis that runs as copies")
            && error.contains(
                "a flat launcher, such as the one `peppy stack resolve` prints, declares no axis \
                 at all: launch the launcher it was resolved from, or take the instance that \
                 serves this target out of the flat file"
            ),
        "{error}"
    );
}

#[test]
fn a_daemon_target_without_a_scope_is_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            r#"{ target: "framework_controls_inst", set_arguments: { port: 8904 } }"#,
        ),
    );
    assert!(
        error.contains("the flat launcher breaks daemon scope rules a launch would reject")
            && error.contains(
                "daemon target `stack` (robot_membership:v1) of instance `framework_controls_inst` \
                 has no scope"
            ),
        "{error}"
    );
}

#[test]
fn a_scope_that_does_not_parse_or_names_no_copy_option_is_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            &scoped(r#"{ max_copies: 4, options: [], model: "so101" }"#),
        ),
    );
    assert!(
        error.contains("does not parse") && error.contains("unknown field `model`"),
        "{error}"
    );

    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            ENDPOINT,
            &scoped(
                r#"{ max_copies: 4, options: [{ option: "none", model: "none", description: "Nothing" }] }"#,
            ),
        ),
    );
    assert!(
        error.contains(
            "`none` is not an option of an axis this launcher runs as copies; the copies it can \
             add:\n  - robot: `openarm_sim`, `so101_sim`"
        ),
        "{error}"
    );
}

#[test]
fn scopes_and_links_of_the_wrong_instance_or_target_are_refused() {
    let (dirs, root) = cold_home();
    let error = refusal(
        &dirs,
        launcher(
            root.path(),
            r#"{ instance_id: "framework_controls_inst", core_node: "robot_pc",
                 links: { stack: "framework_controls_inst" } }"#,
            &scoped(SCOPE),
        ),
    );
    for expected in [
        "instance `framework_controls_inst` has daemon target `fleet` and declares \
         `core_node: \"robot_pc\"`",
        "`links.stack` of instance `framework_controls_inst` names daemon target `stack`, which \
         takes no link",
    ] {
        assert!(
            error.contains(expected),
            "{expected}\nmissing from: {error}"
        );
    }

    // A scope key that names no daemon target of the instance; the one the
    // top-level adjustment replaces holds.
    let stray = launcher(
        root.path(),
        r#"{ instance_id: "framework_controls_inst", arguments: { port: 8903 },
             daemon_scopes: { stack: {}, recorder: {} } }"#,
        &scoped(SCOPE),
    );
    let error = refusal(&dirs, stray);
    assert!(
        error.contains(
            "`daemon_scopes.recorder` of instance `framework_controls_inst` names none of its \
             daemon targets; its daemon targets: `fleet`, `stack`"
        ) && !error.contains("does not parse"),
        "{error}"
    );
}

#[test]
fn a_scope_written_through_a_joined_copy_is_refused() {
    let (dirs, root) = cold_home();
    let path = launcher(
        root.path(),
        ENDPOINT,
        &format!(
            r#"{}, {{ target: "framework_controls_inst", when: {{ robot: "openarm_sim" }},
                   set_daemon_scopes: {{ stack: {SCOPE} }} }}"#,
            scoped(SCOPE)
        ),
    );
    let error = resolve_rendered(
        &dirs,
        path,
        &[],
        &["openarm_sim:alpha".parse().expect("a copy reference")],
    )
    .expect_err("the copy sets a scope")
    .to_string();
    assert!(
        error.contains(
            "copy `alpha` of option `openarm_sim` sets the scope of daemon target `stack` on \
             instance `framework_controls_inst`"
        ) && error.contains("a scope belongs to the stack")
            && !error.contains("would change"),
        "{error}"
    );
}

#[test]
fn a_daemon_interface_this_peppy_does_not_serve_is_refused_by_resolve() {
    let (dirs, root) = cold_home();
    let path = launcher(root.path(), ENDPOINT, &scoped(SCOPE));
    let text = fs::read_to_string(&path)
        .expect("launcher")
        .replace("framework_controls:v1", "unserved_controls:v1");
    fs::write(&path, text).expect("launcher");
    let error = refusal(&dirs, path);
    assert!(
        error.contains("would be refused at launch")
            && error.contains(
                "target `stack`: daemon interface `robot_membership:v2` is not one this peppy \
                 serves; it serves `robot_membership` at tag `v1` only"
            ),
        "{error}"
    );
}

/// An exposure deployment that this machine's caches do not hold.
const ABSENT_DEPLOYMENT: &str = r#"{ source: { exposures: ["absent_controls:v1"] }, instances: [
    { instance_id: "absent_inst", arguments: { port: 8904 } }
] }"#;

/// An exposure that is not in the caches skips its own checks, and the
/// report names it; the checks of every other deployment still run, so a
/// scope another deployment breaks is still refused.
#[test]
fn an_exposure_not_in_the_caches_skips_its_own_checks_and_no_other() {
    let (dirs, root) = cold_home();
    let (_document, report) = resolve_rendered(
        &dirs,
        launcher_with(root.path(), ENDPOINT, &scoped(SCOPE), ABSENT_DEPLOYMENT),
        &[],
        &[],
    )
    .expect("the scope of the cached exposure holds");
    let report = report.join("\n");
    assert!(
        report.contains(
            "daemon scopes not checked for 1 exposure deployment(s) this machine's caches do not \
             hold: exposures [absent_controls:v1]"
        ),
        "{report}"
    );
    assert!(
        report.contains(
            "daemon scopes hold: 2 daemon target(s) scoped and placed on the coordinator"
        ),
        "{report}"
    );

    let error = refusal(
        &dirs,
        launcher_with(
            root.path(),
            r#"{ instance_id: "framework_controls_inst", core_node: "robot_pc" }"#,
            &scoped(SCOPE),
            ABSENT_DEPLOYMENT,
        ),
    );
    assert!(
        error.contains(
            "instance `framework_controls_inst` has daemon target `fleet` and declares \
             `core_node: \"robot_pc\"`"
        ),
        "{error}"
    );
}

/// A document in the caches that a launch refuses fails `stack resolve` with
/// that refusal, and is not reported as unchecked.
#[test]
fn an_exposure_document_that_does_not_parse_fails_resolve() {
    let (dirs, root) = cold_home();
    let path = launcher(root.path(), ENDPOINT, &scoped(SCOPE));
    let text = fs::read_to_string(&path)
        .expect("launcher")
        .replace("framework_controls:v1", "pinned_controls:v1");
    fs::write(&path, text).expect("launcher");
    let error = refusal(&dirs, path);
    assert!(
        error.contains("deployment exposures [pinned_controls:v1] would be refused at launch")
            && error.contains("exposure `pinned_controls:v1` does not parse")
            && error.contains(
                "target `stack` pins daemon interface `robot_membership:v1` with `sha256`"
            ),
        "{error}"
    );
}

/// A launcher whose framework endpoint scopes the robot option
/// `featured_sim` with `features`, the option declaring a brain axis and a
/// commander axis whose `mcp` option brings no instance, under the option's
/// `constraints`.
fn featured_launcher(root: &Path, features: &str, constraints: &str) -> PathBuf {
    let path = root.join("featured.json5");
    fs::write(
        &path,
        format!(
            r#"{{
            peppy_schema: "launcher/v1",
            description: ["A test launcher."],
            components: [
                {{ name: "robot", cardinality: "zero_or_more", options: {{
                    featured_sim: {{
                        deployments: [
                            {{ source: {{ name: "arm", tag: "v1" }}, instances: [{{ instance_id: "arm_inst" }}] }},
                            {{ commander: "mcp" }},
                        ],
                        components: [
                            {{ name: "commander", options: {{
                                mcp: {{}},
                                web: {{ deployments: [{{ source: {{ name: "panel", tag: "v1" }},
                                    instances: [{{ instance_id: "commander_inst" }}] }}] }},
                            }} }},
                            {{ name: "brain", cardinality: "zero_or_one", options: {{
                                ai_brain_vla: {{ deployments: [{{ source: {{ name: "brain", tag: "v1" }},
                                    instances: [{{ instance_id: "brain_inst" }}] }}] }},
                            }} }},
                        ],
                        constraints: [{constraints}],
                    }},
                }} }},
            ],
            deployments: [
                {{ source: {{ exposures: ["framework_controls:v1"] }}, instances: [{ENDPOINT}] }},
            ],
            adjustments: [{{ target: "framework_controls_inst", set_daemon_scopes: {{ stack: {{
                max_copies: 2,
                options: [{{ option: "featured_sim", model: "featured", description: "A featured robot",
                             features: [{features}] }}],
            }}, fleet: {FLEET_SCOPE} }} }}],
        }}"#
        ),
    )
    .expect("launcher");
    path
}

/// `peppy stack resolve` holds the features of a scope to the launch checks:
/// a feature whose axis its option does not declare is refused before any
/// composition, a feature set that does not compose is refused naming the
/// option and the features, and a scope whose feature sets compose
/// resolves.
#[test]
fn resolve_holds_the_features_of_a_scope_to_the_launch_checks() {
    let (dirs, root) = cold_home();
    let brain =
        r#"{ feature: "ai_brain", axis: "brain", option: "ai_brain_vla", description: "A brain" }"#;
    let (_document, report) =
        resolve_rendered(&dirs, featured_launcher(root.path(), brain, ""), &[], &[])
            .expect("the scope and its feature hold");
    assert!(
        report.join("\n").contains("daemon scopes hold"),
        "{report:?}"
    );

    let error = refusal(
        &dirs,
        featured_launcher(
            root.path(),
            r#"{ feature: "ai_brain", axis: "mind", option: "ai_brain_vla", description: "A brain" }"#,
            "",
        ),
    );
    assert!(
        error.contains(
            "feature `ai_brain` of option `featured_sim` selects `mind=ai_brain_vla`, but \
             `featured_sim` has no own axis `mind`; its own axes: `commander`, `brain`"
        ),
        "{error}"
    );

    let error = refusal(
        &dirs,
        featured_launcher(
            root.path(),
            brain,
            r#"{ when: { brain: "ai_brain_vla" }, requires: [{ commander: "web" }],
                 reason: "the brain needs the web panel" }"#,
        ),
    );
    assert!(
        error.contains("option `featured_sim` with the features `ai_brain`: ")
            && error.contains("the brain needs the web panel"),
        "{error}"
    );
}

/// The deployment of `director:v1`, a node whose manifest names the daemon
/// target `fleet` of `robot_leases:v1`.
const DIRECTOR_DEPLOYMENT: &str =
    r#"{ source: { name: "director", tag: "v1" }, instances: [{ instance_id: "director_inst" }] }"#;

/// Writes the manifest of `director:v1` under `root` and records it in the
/// nodes cache of the peppy home at `root`.
fn cache_director(root: &Path) {
    let dir = root.join("director");
    fs::create_dir_all(&dir).expect("node dir");
    fs::write(
        dir.join("peppy.json5"),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "director", tag: "v1", description: ["A test node."], depends_on: {
                daemon: [{ name: "robot_leases", tag: "v1", link_id: "fleet" }],
            } },
            interfaces: { actions: { consumes: [{ link_id: "fleet", name: "lease" }] } },
            execution: { language: "rust", run_cmd: ["./director"] },
        }"#,
    )
    .expect("manifest");
    crate::common::register_repo_caches(root, &[("director", "v1", dir.as_path())]);
}

/// The daemon target of a node is held to the launch checks once its
/// manifest is in the nodes cache; while it is not, the report names the
/// deployment whose checks did not run.
#[test]
fn the_daemon_target_of_a_cached_node_is_checked_and_an_uncached_one_is_reported() {
    let (dirs, root) = cold_home();
    let (_document, report) = resolve_rendered(
        &dirs,
        launcher_with(root.path(), ENDPOINT, &scoped(SCOPE), DIRECTOR_DEPLOYMENT),
        &[],
        &[],
    )
    .expect("the checks of the cached deployments hold");
    let report = report.join("\n");
    assert!(
        report.contains(
            "daemon scopes not checked for 1 node deployment(s) whose manifest this machine's \
             caches do not hold: director:v1 (not in the nodes cache)"
        ),
        "{report}"
    );

    cache_director(root.path());
    let error = refusal(
        &dirs,
        launcher_with(root.path(), ENDPOINT, &scoped(SCOPE), DIRECTOR_DEPLOYMENT),
    );
    assert!(
        error.contains(
            "daemon target `fleet` (robot_leases:v1) of instance `director_inst` has no scope"
        ),
        "{error}"
    );

    let (_document, report) = resolve_rendered(
        &dirs,
        launcher_with(
            root.path(),
            ENDPOINT,
            &format!(
                r#"{}, {{ target: "director_inst", set_daemon_scopes: {{ fleet: {{ members: ["lease"] }} }} }}"#,
                scoped(SCOPE)
            ),
            DIRECTOR_DEPLOYMENT,
        ),
        &[],
        &[],
    )
    .expect("both scopes hold");
    let report = report.join("\n");
    assert!(
        report.contains(
            "daemon scopes hold: 3 daemon target(s) scoped and placed on the coordinator"
        ) && !report.contains("node deployment(s) whose manifest"),
        "{report}"
    );
}

/// The scope of [`SCOPE`] with the product sheet of `so101` given by an
/// include of `products/so101.json5`, beside the launcher.
const SCOPE_WITH_PRODUCT: &str = r#"{
    max_copies: 4,
    options: [
        { option: "openarm_sim", model: "openarm_v2", description: "A simulated OpenArm v2 standing on the floor" },
        { option: "so101_sim", model: "so101", description: "A simulated SO-101 clamped on the edge of a table",
          product: { $include: "products/so101.json5" } },
    ],
}"#;

/// The product sheet of `so101`, with its two dates `checked_on`.
fn so101_sheet(checked_on: &str) -> String {
    format!(
        r#"{{
        vendor: "A seller",
        purchase_url: "https://shop.example.com/so101",
        price: {{ amount: 130, currency: "USD", checked_on: "{checked_on}" }},
        availability: {{ state: "in_stock", checked_on: "{checked_on}" }},
        mass_kg: 1.4,
    }}"#
    )
}

/// `peppy stack resolve` reads the included product sheet into the scope,
/// and the flat launcher carries its value.
#[test]
fn resolve_reads_the_product_sheet_that_the_launcher_includes() {
    let (dirs, root) = cold_home();
    fs::create_dir_all(root.path().join("products")).expect("products dir");
    fs::write(
        root.path().join("products/so101.json5"),
        so101_sheet("2026-10-01"),
    )
    .expect("sheet");
    let (document, _report) = resolve_rendered(
        &dirs,
        launcher(root.path(), ENDPOINT, &scoped(SCOPE_WITH_PRODUCT)),
        &[],
        &[],
    )
    .expect("the scope holds");
    assert!(
        document.contains("https://shop.example.com/so101") && !document.contains("$include"),
        "the flat launcher carries the value of the include: {document}"
    );
}

/// `peppy stack resolve` holds the dates of a product sheet to the latest date
/// on Earth: a date of the year 9999 is in the future on any machine that runs
/// this test, and the refusal names the option, the model and the field. A
/// launch only warns about such a date, so the refusal says so.
#[test]
fn resolve_refuses_a_product_sheet_dated_in_the_future() {
    let (dirs, root) = cold_home();
    fs::create_dir_all(root.path().join("products")).expect("products dir");
    fs::write(
        root.path().join("products/so101.json5"),
        so101_sheet("9999-12-31"),
    )
    .expect("sheet");
    let error = refusal(
        &dirs,
        launcher(root.path(), ENDPOINT, &scoped(SCOPE_WITH_PRODUCT)),
    );
    assert!(
        error.contains(
            "the daemon scopes give dates after the latest date on Earth, which a launch warns \
             about"
        ) && !error.contains("a launch would reject"),
        "{error}"
    );
    for field in ["price", "availability"] {
        assert!(
            error.contains(&format!(
                "the scope of daemon target `stack` (robot_membership:v1) on instance \
                 `framework_controls_inst` gives a date in the future: options[1].product: the \
                 product sheet of model `so101`: {field}.checked_on: `9999-12-31` is in the \
                 future"
            )),
            "{error}"
        );
    }
}
