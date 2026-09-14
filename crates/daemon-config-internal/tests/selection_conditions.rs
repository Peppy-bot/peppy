use daemon_config::launcher::{PeppyLauncherParser, PreparedLauncher};
use std::path::Path;

fn prepared(constraint: &str) -> PreparedLauncher {
    let document = format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{ name: "robot", options: {{ real: {{}}, sim: {{}}, other: {{}} }} }},
            {{ name: "commander", options: {{ web: {{}}, xr: {{}}, mcp: {{}} }} }}
        ],
        deployments: [
            {{ source: {{ name: "arm", tag: "v1" }}, instances: [
                {{ instance_id: "arm_inst", arguments: {{ speed: 1 }} }}
            ] }},
            {{ robot: "real" }},
            {{ commander: "web" }},
        ],
        adjustments: [{{ target: "arm_inst",
            when: {{ robot: ["real", "sim"], commander: ["web", "xr"] }},
            set_arguments: {{ speed: 2 }}
        }}],
        constraints: [{constraint}]
    }}"#
    );
    let launcher = PeppyLauncherParser::from_content(&document).unwrap();
    PreparedLauncher::load(&launcher, Path::new("fleet.json5")).unwrap()
}

#[test]
fn adjustments_match_any_option_on_every_named_axis() {
    let launcher = prepared("");
    for robot in ["real", "sim", "other"] {
        for commander in ["web", "xr", "mcp"] {
            let flat = launcher
                .launch(&[robot.into(), commander.into()])
                .unwrap()
                .launcher;
            let encoded = serde_json::to_value(flat).unwrap();
            assert_eq!(
                encoded["deployments"][0]["instances"][0]["arguments"]["speed"],
                if robot != "other" && commander != "mcp" {
                    2
                } else {
                    1
                }
            );
        }
    }
}

#[test]
fn constraint_lists_use_the_same_matching_as_adjustments() {
    for constraint in [
        r#"{ when: { robot: ["real", "sim"] }, requires: [{ commander: ["web", "xr"] }], reason: "select web or xr" }"#,
        r#"{ when: { commander: ["web", "xr", "mcp"] }, forbids: [{ commander: ["mcp"], robot: ["real", "sim"] }], reason: "select web or xr" }"#,
    ] {
        let launcher = prepared(constraint);
        for robot in ["real", "sim"] {
            for commander in ["web", "xr", "mcp"] {
                let result = launcher.launch(&[robot.into(), commander.into()]);
                assert_eq!(
                    result.is_ok(),
                    commander != "mcp",
                    "{robot} {commander}: {result:?}"
                );
            }
        }
    }
}
