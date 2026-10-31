//! `$include` in launcher documents: where an include resolves, and every
//! include that peppy refuses, named by its document, its JSON path, its
//! include path and the reason.

use daemon_config::launcher::{LauncherFragmentParser, PeppyLauncherParser, PreparedLauncher};
use serde_json::{Value, json};
use std::{fs, path::Path};

mod common;
use common::{fragment_file, write};

fn write_repository_index(root: &Path) {
    write(
        &root.join("peppy_repository.json5"),
        r#"{ peppy_schema: "repository/v1" }"#,
    );
}

/// A flat launcher whose one instance takes `arguments` as written.
fn launcher_with_arguments(arguments: &str) -> String {
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        deployments: [{{ source: {{ name: "controller", tag: "v1" }},
            instances: [{{ instance_id: "controller_inst", arguments: {arguments} }}] }}],
    }}"#
    )
}

/// The refusal of the launcher at `file`, written with `arguments`.
fn refusal_of(file: &Path, arguments: &str) -> String {
    write(file, &launcher_with_arguments(arguments));
    PeppyLauncherParser::from_path(file)
        .expect_err("the include is refused")
        .to_string()
}

/// The arguments of the first instance of `launcher`, as JSON.
fn first_arguments(launcher: &daemon_config::launcher::PeppyLauncher) -> Value {
    serde_json::to_value(&launcher.deployments[0].instances[0].arguments).unwrap()
}

#[test]
fn a_launcher_takes_the_value_of_each_included_file() {
    let root = tempfile::tempdir().unwrap();
    write_repository_index(root.path());
    write(
        &root.path().join("shared/description.json5"),
        r#"["A launcher whose parts live in other files."]"#,
    );
    write(
        &root.path().join("launchers/controller_arguments.json5"),
        "// The arguments of the controller.\n{ rate_hz: 30, mode: \"teleop\" }",
    );
    write(
        &root.path().join("products/so101.json5"),
        r#"{ vendor: "A seller", mass_kg: 1.5 }"#,
    );
    let launcher = write(
        &root.path().join("launchers/teleop.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: { $include: "../shared/description.json5" },
        deployments: [{ source: { name: "controller", tag: "v1" },
            instances: [{
                instance_id: "controller_inst",
                arguments: { $include: "controller_arguments.json5" },
                daemon_scopes: { stack: { max_copies: 2, options: [{
                    option: "so101_sim", model: "so101", description: "One SO-101",
                    product: { $include: "../products/so101.json5" },
                }] } },
            }] }],
    }"#,
    );

    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(
        parsed.description.summary().as_str(),
        "A launcher whose parts live in other files."
    );
    assert_eq!(
        first_arguments(&parsed),
        json!({ "mode": "teleop", "rate_hz": 30 }),
        "the include of an argument object, relative to the launcher's directory"
    );
    assert_eq!(
        parsed.deployments[0].instances[0].daemon_scopes["stack"]["options"][0]["product"],
        json!({ "vendor": "A seller", "mass_kg": 1.5 }),
        "a scope holds the value of its include, which the scope parser reads"
    );
}

#[test]
fn an_include_takes_any_json_value_in_place_of_its_object() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("rate.json5"), "30");
    write(&root.path().join("mode.json5"), "'teleop'");
    write(&root.path().join("gains.json5"), "[1, 2.5]");
    write(&root.path().join("nothing.json5"), "null");
    let launcher = root.path().join("teleop.json5");
    write(
        &launcher,
        &launcher_with_arguments(
            r#"{ rate_hz: { $include: "rate.json5" }, mode: { $include: "mode.json5" },
                 gains: { $include: "gains.json5" }, spare: { $include: "nothing.json5" } }"#,
        ),
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(
        first_arguments(&parsed),
        json!({ "gains": [1, 2.5], "mode": "teleop", "rate_hz": 30, "spare": null })
    );
}

#[test]
fn an_include_in_a_fragment_resolves_against_the_fragment_directory() {
    let root = tempfile::tempdir().unwrap();
    write_repository_index(root.path());
    write(
        &root.path().join("commanders/xr_arguments.json5"),
        r#"{ hand: "left" }"#,
    );
    write(
        &root.path().join("commanders/xr.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "controller", tag: "v1" },
                instances: [{ instance_id: "controller_inst",
                              arguments: { $include: "xr_arguments.json5" } }] }]"#,
        ),
    );
    let launcher = write(
        &root.path().join("openarm/fleet.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [{ name: "commander", options: { xr: "../commanders/xr.json5" } }],
        deployments: [{ commander: "xr" }],
    }"#,
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    let prepared = PreparedLauncher::load(&parsed, &launcher).expect("the fragments load");
    let flat = prepared.launch(&[], &[]).expect("composes").launcher;
    assert_eq!(first_arguments(&flat), json!({ "hand": "left" }));

    let alone = LauncherFragmentParser::from_path(root.path().join("commanders/xr.json5"))
        .expect("a fragment read alone resolves its includes too");
    assert_eq!(
        serde_json::to_value(&alone.body.deployments[0].instances[0].arguments).unwrap(),
        json!({ "hand": "left" })
    );
}

#[test]
fn an_include_of_a_fragment_stays_inside_the_launcher_repository() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repository");
    write_repository_index(&root);
    write(&outer.path().join("outside.json5"), r#"{ hand: "left" }"#);
    write(
        &root.join("commanders/xr.json5"),
        &fragment_file(
            r#"deployments: [{ source: { name: "controller", tag: "v1" },
                instances: [{ instance_id: "controller_inst",
                              arguments: { $include: "../../outside.json5" } }] }]"#,
        ),
    );
    let launcher = write(
        &root.join("openarm/fleet.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        components: [{ name: "commander", options: { xr: "../commanders/xr.json5" } }],
        deployments: [{ commander: "xr" }],
    }"#,
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    let error = PreparedLauncher::load(&parsed, &launcher)
        .expect_err("the include of the fragment is refused")
        .to_string();
    let fragment = root.join("commanders/xr.json5").canonicalize().unwrap();
    assert!(
        error.contains(&format!(
            "{}: the `$include` of `../../outside.json5` at \
             `deployments[0].instances[0].arguments` is refused: it leaves the launcher's \
             repository",
            fragment.display()
        )),
        "the refusal names the fragment file and the include in it: {error}"
    );
    assert!(
        error.contains("fragment `../commanders/xr.json5`"),
        "{error}"
    );
}

#[test]
fn an_include_that_leaves_the_repository_is_refused_with_its_place() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repository");
    write_repository_index(&root);
    write(&outer.path().join("outside.json5"), "{}");
    write(&root.join("shared/arguments.json5"), "{}");
    let launcher = root.join("launchers/teleop.json5");
    for include in [
        "../../outside.json5",
        "../../repository/shared/arguments.json5",
    ] {
        let error = refusal_of(&launcher, &format!(r#"{{ $include: "{include}" }}"#));
        assert_eq!(
            error,
            format!(
                "{}: the `$include` of `{include}` at `deployments[0].instances[0].arguments` is \
                 refused: it leaves the launcher's repository; an include path is relative to \
                 the directory of the document that holds the include, and it stays inside the \
                 launcher's repository: the directory of the nearest `peppy_repository.json5` \
                 above the launcher, or the launcher's own directory for a standalone launcher",
                launcher.display()
            )
        );
    }
}

#[test]
fn a_standalone_launcher_includes_files_of_its_own_directory_only() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("shared/arguments.json5"), "{}");
    write(
        &root.path().join("robot/arguments.json5"),
        r#"{ hand: "left" }"#,
    );
    let launcher = root.path().join("robot/teleop.json5");
    let error = refusal_of(&launcher, r#"{ $include: "../shared/arguments.json5" }"#);
    assert!(
        error.contains("it leaves the launcher's repository") && error.contains("standalone"),
        "{error}"
    );
    write(
        &launcher,
        &launcher_with_arguments(r#"{ $include: "arguments.json5" }"#),
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(first_arguments(&parsed), json!({ "hand": "left" }));
}

#[test]
fn an_include_path_is_a_plain_relative_path() {
    let root = tempfile::tempdir().unwrap();
    let arguments = write(&root.path().join("shared/arguments.json5"), "{}");
    let launcher = root.path().join("teleop.json5");
    for include in ["./shared/arguments.json5", "shared/./arguments.json5"] {
        let error = refusal_of(&launcher, &format!(r#"{{ $include: "{include}" }}"#));
        assert!(
            error.contains(&format!(
                "the `$include` of `{include}` at `deployments[0].instances[0].arguments` is \
                 refused: it contains a `.` segment"
            )),
            "{error}"
        );
    }
    let absolute = arguments.display().to_string();
    let error = refusal_of(&launcher, &format!(r#"{{ $include: "{absolute}" }}"#));
    assert!(
        error.contains(&format!(
            "the `$include` of `{absolute}` at `deployments[0].instances[0].arguments` is \
             refused: it is absolute"
        )),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn an_include_cannot_escape_through_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_repository_index(root.path());
    write(&outside.path().join("arguments.json5"), "{}");
    std::os::unix::fs::symlink(
        outside.path().join("arguments.json5"),
        root.path().join("arguments.json5"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("shared")).unwrap();
    let launcher = root.path().join("launchers/teleop.json5");
    for include in ["../arguments.json5", "../shared/arguments.json5"] {
        let error = refusal_of(&launcher, &format!(r#"{{ $include: "{include}" }}"#));
        assert!(
            error.contains("it leaves the launcher's repository through a symlink"),
            "{include}: {error}"
        );
    }
}

#[test]
fn an_include_object_holds_one_string_key() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("arguments.json5"), "{}");
    let launcher = root.path().join("teleop.json5");
    let error = refusal_of(
        &launcher,
        r#"{ $include: "arguments.json5", rate_hz: 30, mode: "x" }"#,
    );
    assert!(
        error.ends_with(
            "the `$include` of `arguments.json5` at `deployments[0].instances[0].arguments` is \
             refused: an include object holds `$include` alone, and this one also holds \
             `rate_hz`, `mode`"
        ),
        "{error}"
    );
    let error = refusal_of(&launcher, r#"{ $include: 42 }"#);
    assert!(
        error.ends_with(
            "the `$include` of `42` at `deployments[0].instances[0].arguments` is refused: \
             `$include` takes the path of a JSON5 file, as a string"
        ),
        "{error}"
    );
    let error = refusal_of(&launcher, r#"{ $include: ["arguments.json5"] }"#);
    assert!(
        error.contains(r#"the `$include` of `["arguments.json5"]`"#),
        "{error}"
    );
}

#[test]
fn an_included_file_includes_no_other_file() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("gains.json5"), "[1, 2]");
    write(
        &root.path().join("arguments.json5"),
        r#"{ control: { gains: { $include: "gains.json5" } } }"#,
    );
    let error = refusal_of(
        &root.path().join("teleop.json5"),
        r#"{ $include: "arguments.json5" }"#,
    );
    assert!(
        error.ends_with(
            "the `$include` of `arguments.json5` at `deployments[0].instances[0].arguments` is \
             refused: the included file holds an `$include` at `control.gains`, and an included \
             file includes no other file"
        ),
        "{error}"
    );
}

#[test]
fn an_unreadable_or_unparsable_included_file_is_refused() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("folder.json5")).unwrap();
    write(&root.path().join("broken.json5"), "{ rate_hz: }");
    write(&root.path().join("empty.json5"), "  \n");
    write(
        &root.path().join("repeated.json5"),
        "{ rate_hz: 1, rate_hz: 2 }",
    );
    let launcher = root.path().join("teleop.json5");
    for (include, reason) in [
        (
            "missing.json5",
            "it cannot be read: No such file or directory",
        ),
        ("folder.json5", "it cannot be read: Is a directory"),
        ("broken.json5", "it does not parse as JSON5: "),
        ("empty.json5", "the file is empty"),
        (
            "repeated.json5",
            "it does not parse as JSON5: the key `rate_hz` is written twice in one object",
        ),
    ] {
        let error = refusal_of(&launcher, &format!(r#"{{ $include: "{include}" }}"#));
        assert!(
            error.contains(&format!(
                "the `$include` of `{include}` at `deployments[0].instances[0].arguments` is \
                 refused: {reason}"
            )),
            "{include}: {error}"
        );
    }
}

#[test]
fn a_document_read_from_text_holds_no_include() {
    let error = PeppyLauncherParser::from_content(&launcher_with_arguments(
        r#"{ $include: "arguments.json5" }"#,
    ))
    .expect_err("the include is refused")
    .to_string();
    assert_eq!(
        error,
        "the document read from text: the `$include` of `arguments.json5` at \
         `deployments[0].instances[0].arguments` is refused: the document is read from text and \
         has no file path, and an include resolves against the directory of its document"
    );
    let error = LauncherFragmentParser::from_content(&fragment_file(
        r#"deployments: [{ source: { name: "controller", tag: "v1" },
            instances: [{ instance_id: "controller_inst",
                          arguments: { $include: "arguments.json5" } }] }]"#,
    ))
    .expect_err("the include is refused")
    .to_string();
    assert!(
        error.contains("the document is read from text and has no file path"),
        "{error}"
    );
}

/// The JSON path of an include names where it is in the document, under an
/// adjustment as well.
#[test]
fn a_refusal_names_the_json_path_of_the_include() {
    let root = tempfile::tempdir().unwrap();
    let launcher = write(
        &root.path().join("teleop.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        adjustments: [
            { target: "a", set_arguments: { x: 1 } },
            { target: "b", set_arguments: { x: 2 } },
            { target: "framework_controls_inst", set_daemon_scopes: { stack: { max_copies: 2,
                options: [
                    { option: "openarm_sim", model: "openarm_v2", description: "An OpenArm" },
                    { option: "so101_sim", model: "so101", description: "An SO-101",
                      product: { $include: "../so101/product.json5" } },
                ] } } },
        ],
    }"#,
    );
    let error = PeppyLauncherParser::from_path(&launcher)
        .expect_err("the include is refused")
        .to_string();
    assert!(
        error.starts_with(&format!(
            "{}: the `$include` of `../so101/product.json5` at \
             `adjustments[2].set_daemon_scopes.stack.options[1].product` is refused: it leaves \
             the launcher's repository",
            launcher.display()
        )),
        "{error}"
    );
}

/// A document that holds an include is read with each key of each object
/// written once, as the typed parse of a document with no include reads
/// the fields of its objects.
#[test]
fn a_document_with_an_include_writes_each_key_once() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("arguments.json5"), "{}");
    let launcher = write(
        &root.path().join("teleop.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        deployments: [{ source: { name: "controller", tag: "v1" },
            instances: [{ instance_id: "controller_inst", instance_id: "again",
                          arguments: { $include: "arguments.json5" } }] }],
    }"#,
    );
    let error = PeppyLauncherParser::from_path(&launcher)
        .expect_err("the repeated key is refused")
        .to_string();
    assert!(
        error.contains(
            "deployments[0].instances[0]: the key `instance_id` is written twice in one object"
        ),
        "{error}"
    );
}

/// The typed parse runs on the resolved document, so an included value
/// that does not fit its place is refused as the same value written there.
#[test]
fn an_included_value_is_parsed_at_its_place_in_the_document() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("instance_id.json5"), "42");
    let launcher = write(
        &root.path().join("teleop.json5"),
        r#"{
        peppy_schema: "launcher/v1",
        description: ["A test launcher."],
        deployments: [{ source: { name: "controller", tag: "v1" },
            instances: [{ instance_id: { $include: "instance_id.json5" } }] }],
    }"#,
    );
    let error = PeppyLauncherParser::from_path(&launcher)
        .expect_err("a number is no instance id")
        .to_string();
    write(
        &launcher,
        &fs::read_to_string(&launcher)
            .unwrap()
            .replace(r#"{ $include: "instance_id.json5" }"#, "42"),
    );
    let written = PeppyLauncherParser::from_path(&launcher)
        .expect_err("a number is no instance id")
        .to_string();
    assert_eq!(error, written);
    assert!(error.contains("integer `42`"), "{error}");
}

/// A document that holds an include is parsed from its resolved value, so
/// every string that JSON5 writes reaches the typed document as written:
/// the line and paragraph separators U+2028 and U+2029 too, in the document
/// and in the included file.
#[test]
fn a_document_with_an_include_keeps_each_character_of_its_strings() {
    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("gains.json5"),
        r#"{ label: "three\u2029four" }"#,
    );
    let launcher = root.path().join("teleop.json5");
    write(
        &launcher,
        &launcher_with_arguments(r#"{ note: "one\u2028two", gains: { $include: "gains.json5" } }"#),
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(
        first_arguments(&parsed),
        json!({ "note": "one\u{2028}two", "gains": { "label": "three\u{2029}four" } })
    );
}

/// The key `$include` written with an escape is the key `$include`.
#[test]
fn an_include_key_written_with_an_escape_is_an_include() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("rate.json5"), "30");
    let launcher = root.path().join("teleop.json5");
    write(
        &launcher,
        &launcher_with_arguments(r#"{ rate_hz: { "\u0024include": "rate.json5" } }"#),
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(first_arguments(&parsed), json!({ "rate_hz": 30 }));
}

/// A document that writes `$include` in a string and holds no include
/// object is read as a document with no include.
#[test]
fn the_text_of_an_include_key_in_a_string_is_no_include() {
    let root = tempfile::tempdir().unwrap();
    let launcher = root.path().join("teleop.json5");
    write(
        &launcher,
        &launcher_with_arguments(r#"{ note: "write { $include: path } to include a file" }"#),
    );
    let parsed = PeppyLauncherParser::from_path(&launcher).expect("the launcher parses");
    assert_eq!(
        first_arguments(&parsed),
        json!({ "note": "write { $include: path } to include a file" })
    );
}
