use daemon_config::launcher::{PeppyLauncherParser, PreparedLauncher};
use std::{fs, path::Path};

mod common;
use common::{fragment_file, write};

fn repository(path: &Path) {
    write(
        &path.join("peppy_repository.json5"),
        r#"{ peppy_schema: "repository/v1" }"#,
    );
}

fn fragment(path: &Path) {
    write(
        path,
        &fragment_file(
            r#"deployments: [{ source: { name: "controller", tag: "v1" },
                instances: [{ instance_id: "controller_inst" }] }]"#,
        ),
    );
}

fn prepare(file: &Path, reference: &str) -> Result<PreparedLauncher, String> {
    let document = format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [{{ name: "commander", options: {{ xr: {} }} }}],
        deployments: [{{ commander: "xr" }}],
    }}"#,
        serde_json::to_string(reference).unwrap()
    );
    let parsed = PeppyLauncherParser::from_content(&document).unwrap();
    PreparedLauncher::load(&parsed, file).map_err(|error| error.to_string())
}

#[test]
fn a_robot_launcher_shares_sibling_fragments_inside_its_repository() {
    let root = tempfile::tempdir().unwrap();
    repository(root.path());
    fs::create_dir(root.path().join("openarm")).unwrap();
    fragment(&root.path().join("commanders/xr.json5"));
    let launcher = root.path().join("openarm/fleet.json5");
    let prepared = prepare(&launcher, "../commanders/xr.json5").unwrap();
    let flat = prepared.launch(&[]).unwrap().launcher;
    assert_eq!(
        flat.deployments[0].instances[0].instance_id.as_str(),
        "controller_inst"
    );
    fs::remove_file(root.path().join("commanders/xr.json5")).unwrap();
    assert!(
        prepared.launch(&[]).is_ok(),
        "fragment contents are snapshotted"
    );
}

#[test]
fn standalone_launchers_remain_confined_to_their_own_directory() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("robot")).unwrap();
    fragment(&root.path().join("shared/controller.json5"));
    let error = prepare(
        &root.path().join("robot/fleet.json5"),
        "../shared/controller.json5",
    )
    .unwrap_err();
    assert!(
        error.contains("leaves the launcher's repository"),
        "{error}"
    );
    assert!(error.contains("standalone"), "{error}");
}

#[test]
fn parent_segments_cannot_leave_and_reenter_the_repository() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("repository");
    repository(&root);
    fs::create_dir(root.join("robot")).unwrap();
    fragment(&root.join("shared/controller.json5"));
    fragment(&outer.path().join("outside.json5"));
    for reference in [
        "../../outside.json5",
        "../../repository/shared/controller.json5",
    ] {
        let error = prepare(&root.join("robot/fleet.json5"), reference).unwrap_err();
        assert!(
            error.contains("leaves the launcher's repository"),
            "{error}"
        );
    }
}

#[test]
fn the_nearest_repository_index_sets_the_boundary() {
    let outer = tempfile::tempdir().unwrap();
    repository(outer.path());
    fragment(&outer.path().join("shared/controller.json5"));
    let inner = outer.path().join("nested");
    repository(&inner);
    fs::create_dir(inner.join("robot")).unwrap();
    let error = prepare(
        &inner.join("robot/fleet.json5"),
        "../../shared/controller.json5",
    )
    .unwrap_err();
    assert!(
        error.contains("leaves the launcher's repository"),
        "{error}"
    );
}

#[test]
fn an_invalid_repository_index_is_reported_at_its_path() {
    let root = tempfile::tempdir().unwrap();
    write(
        &root.path().join("peppy_repository.json5"),
        r#"{ peppy_schema: "repository/v999" }"#,
    );
    fs::create_dir(root.path().join("robot")).unwrap();
    fragment(&root.path().join("shared/controller.json5"));
    let error = prepare(
        &root.path().join("robot/fleet.json5"),
        "../shared/controller.json5",
    )
    .unwrap_err();
    assert!(error.contains("peppy_repository.json5"), "{error}");
    assert!(error.contains("repository/v999"), "{error}");
}

#[cfg(unix)]
#[test]
fn repository_fragments_cannot_escape_through_file_or_directory_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    repository(root.path());
    fs::create_dir(root.path().join("robot")).unwrap();
    fragment(&outside.path().join("controller.json5"));
    std::os::unix::fs::symlink(
        outside.path().join("controller.json5"),
        root.path().join("controller.json5"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("shared")).unwrap();
    for reference in ["../controller.json5", "../shared/controller.json5"] {
        let error = prepare(&root.path().join("robot/fleet.json5"), reference).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn internal_symlinks_keep_the_same_repository_boundary() {
    let root = tempfile::tempdir().unwrap();
    repository(root.path());
    fs::create_dir(root.path().join("robot")).unwrap();
    fragment(&root.path().join("shared/controller.json5"));
    std::os::unix::fs::symlink(root.path().join("shared"), root.path().join("alias")).unwrap();
    assert!(
        prepare(
            &root.path().join("robot/fleet.json5"),
            "../alias/controller.json5"
        )
        .is_ok()
    );
}
