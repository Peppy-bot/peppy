//! `public-peppy-libs` is a sealed tree: nothing inside it may reach outside it.
//!
//! The tree lives inside the `peppy` repository, next to the `peppy` workspace
//! (`crates/`), and the dependency between the two runs one way only. The
//! `peppy` crates depend on the crates here; the crates here depend on each
//! other and on the registry, never on `crates/` or anything else beyond the
//! tree's root. That is what lets `platform-backend` and the hub nodes consume
//! these crates from the repository without building a line of `peppy` itself.
//!
//! This suite holds every manifest in the tree to that rule, so a reach outside
//! fails here with the offending manifest and key named. It reads manifests and
//! symlinks only. A source-level reach (`#[path]`, `include!`) is caught by CI
//! instead, which runs the tree's suites from a copy holding nothing but the
//! tree, where such a path has nothing to resolve to.
//!
//! A manifest can also reach outside without writing a path: a package that
//! neither is a workspace root nor names one sends cargo looking for a root in
//! every directory above it, past the tree's own. The suite holds every package
//! to a root inside the tree, so the tree builds the same under any parent.
//!
//! The test lives in `build-helpers` because this is the crate that already
//! knows the tree's layout (`peppy_shared_dir`) and sits at the bottom of its
//! dependency graph.

use std::path::{Component, Path, PathBuf};

use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Directories that tools create inside the tree and git ignores. They hold no
/// manifest of ours and are free to leave the tree: pixi's detached
/// environments are a symlink out of it by design, and CI links `target` to a
/// cache disk.
const TOOL_DIRS: &[&str] = &[
    ".git",
    ".pixi",
    ".venv",
    "__pycache__",
    "node_modules",
    "target",
];

/// One filesystem reference in a manifest that resolves outside the tree.
#[derive(Debug, PartialEq)]
struct Escape {
    /// Dotted trail of the key holding the reference, e.g. `dependencies.auth.path`.
    key: String,
    /// The reference exactly as the manifest spells it.
    reference: String,
}

/// The root of the sealed tree: the directory holding `peppy-shared`.
fn tree_root() -> PathBuf {
    build_helpers::peppy_shared_dir()
        .parent()
        .expect("peppy-shared sits inside the public-peppy-libs tree")
        .to_path_buf()
}

/// Resolves `.` and `..` without touching the filesystem, so a reference is
/// judged by where it points whether or not anything exists there.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized
}

/// Whether `reference`, written relative to `base_dir`, stays inside `root`.
fn stays_inside(root: &Path, base_dir: &Path, reference: &str) -> bool {
    normalize(&base_dir.join(reference)).starts_with(root)
}

/// Whether the string at `trail` names a place on disk. Every `path` key does,
/// wherever it sits: dependencies of each kind (target-specific and
/// `[workspace.dependencies]` included), `[patch]`, `[replace]`, and the source
/// file of a `[lib]`, `[[bin]]` or `[[test]]` target. Beyond those, a package
/// can adopt a workspace root or a build script by location, and a workspace
/// lists its members by location.
fn is_filesystem_reference(trail: &[String]) -> bool {
    let trail: Vec<&str> = trail.iter().map(String::as_str).collect();
    matches!(
        trail.as_slice(),
        [.., "path"]
            | ["package", "workspace"]
            | ["package", "build"]
            | ["workspace", "members"]
            | ["workspace", "exclude"]
    )
}

fn check_string(
    root: &Path,
    manifest_dir: &Path,
    trail: &[String],
    reference: &str,
    escapes: &mut Vec<Escape>,
) {
    if is_filesystem_reference(trail) && !stays_inside(root, manifest_dir, reference) {
        escapes.push(Escape {
            key: trail.join("."),
            reference: reference.to_string(),
        });
    }
}

fn walk_value(
    root: &Path,
    manifest_dir: &Path,
    trail: &mut Vec<String>,
    value: &Value,
    escapes: &mut Vec<Escape>,
) {
    match value {
        Value::String(reference) => {
            check_string(root, manifest_dir, trail, reference.value(), escapes);
        }
        // An array's entries answer to the array's own key (`members = [...]`).
        Value::Array(array) => {
            for entry in array {
                walk_value(root, manifest_dir, trail, entry, escapes);
            }
        }
        Value::InlineTable(table) => walk_table(root, manifest_dir, trail, table, escapes),
        _ => {}
    }
}

fn walk_table(
    root: &Path,
    manifest_dir: &Path,
    trail: &mut Vec<String>,
    table: &dyn TableLike,
    escapes: &mut Vec<Escape>,
) {
    for (key, item) in table.iter() {
        trail.push(key.to_string());
        match item {
            Item::Value(value) => walk_value(root, manifest_dir, trail, value, escapes),
            Item::Table(table) => walk_table(root, manifest_dir, trail, table, escapes),
            Item::ArrayOfTables(tables) => {
                for table in tables {
                    walk_table(root, manifest_dir, trail, table, escapes);
                }
            }
            Item::None => {}
        }
        trail.pop();
    }
}

fn parse_manifest(manifest_dir: &Path, manifest: &str) -> DocumentMut {
    manifest
        .parse()
        .unwrap_or_else(|error| panic!("manifest in {manifest_dir:?} is not valid TOML: {error}"))
}

/// Every filesystem reference in `manifest` that leaves `root`, given the
/// manifest sits in `manifest_dir`.
fn escapes_in(root: &Path, manifest_dir: &Path, manifest: &str) -> Vec<Escape> {
    let document = parse_manifest(manifest_dir, manifest);
    let mut escapes = Vec::new();
    walk_table(
        root,
        manifest_dir,
        &mut Vec::new(),
        document.as_table(),
        &mut escapes,
    );
    escapes
}

/// Every `Cargo.toml` and every symlink under `root`, tool directories aside.
fn manifests_and_symlinks(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut manifests = Vec::new();
    let mut symlinks = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("cannot read {dir:?}: {error}"));
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| panic!("cannot read {dir:?}: {error}"));
            let path = entry.path();
            // By name before by type: a tool directory may itself be a symlink.
            if TOOL_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let file_type = entry
                .file_type()
                .unwrap_or_else(|error| panic!("cannot stat {path:?}: {error}"));
            if file_type.is_symlink() {
                symlinks.push(path);
            } else if file_type.is_dir() {
                stack.push(path);
            } else if entry.file_name() == "Cargo.toml" {
                manifests.push(path);
            }
        }
    }
    manifests.sort();
    symlinks.sort();
    (manifests, symlinks)
}

/// Whether `document` is a workspace root (`[workspace]`).
fn is_workspace_root(document: &DocumentMut) -> bool {
    document.get("workspace").is_some()
}

/// Whether `document` names its workspace root (`package.workspace`), which
/// spares cargo the search. Where the key points is a filesystem reference,
/// held inside the tree with the rest of them.
fn names_its_workspace_root(document: &DocumentMut) -> bool {
    document
        .get("package")
        .and_then(|package| package.get("workspace"))
        .is_some()
}

/// The directories, among `manifests` (each a directory and the contents of
/// the manifest in it), of the packages whose workspace root cargo looks for
/// outside `root`. Cargo takes the root a package names, else the package
/// itself when it is one, else the nearest one in the directories above. A
/// package is adrift when none of those is found from its directory up to
/// `root`: what it builds against is then whatever manifest sits above the
/// tree, and under a workspace that does not list it, it does not build.
fn packages_adrift(root: &Path, manifests: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let documents: Vec<(&Path, DocumentMut)> = manifests
        .iter()
        .map(|(manifest_dir, contents)| {
            (
                manifest_dir.as_path(),
                parse_manifest(manifest_dir, contents),
            )
        })
        .collect();
    let workspace_roots: Vec<&Path> = documents
        .iter()
        .filter(|(_, document)| is_workspace_root(document))
        .map(|(manifest_dir, _)| *manifest_dir)
        .collect();
    let has_a_root_above_it_inside_the_tree = |manifest_dir: &Path| {
        manifest_dir
            .ancestors()
            .take_while(|dir| dir.starts_with(root))
            .any(|dir| workspace_roots.contains(&dir))
    };

    documents
        .iter()
        .filter(|(_, document)| !names_its_workspace_root(document))
        .filter(|(_, document)| !is_workspace_root(document))
        .filter(|(manifest_dir, _)| !has_a_root_above_it_inside_the_tree(manifest_dir))
        .map(|(manifest_dir, _)| manifest_dir.to_path_buf())
        .collect()
}

/// Every manifest of the tree, as the directory it sits in and its contents.
fn tree_manifests(root: &Path) -> Vec<(PathBuf, String)> {
    let (manifests, _) = manifests_and_symlinks(root);

    // The walk must have covered the tree for a clean result to mean anything.
    let own_manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert!(
        manifests.contains(&own_manifest),
        "the walk from {root:?} did not reach {own_manifest:?}"
    );

    manifests
        .iter()
        .map(|manifest| {
            let manifest_dir = manifest
                .parent()
                .expect("a manifest has a parent directory")
                .to_path_buf();
            let contents = std::fs::read_to_string(manifest)
                .unwrap_or_else(|error| panic!("cannot read {manifest:?}: {error}"));
            (manifest_dir, contents)
        })
        .collect()
}

/// `manifest_dir`'s manifest as the failure messages name it: from the tree's
/// root down.
fn display_manifest(root: &Path, manifest_dir: &Path) -> String {
    let manifest = manifest_dir.join("Cargo.toml");
    manifest
        .strip_prefix(root)
        .unwrap_or(&manifest)
        .display()
        .to_string()
}

#[test]
fn no_manifest_reaches_outside_the_tree() {
    let root = tree_root();

    let mut violations = Vec::new();
    for (manifest_dir, contents) in &tree_manifests(&root) {
        for escape in escapes_in(&root, manifest_dir, contents) {
            violations.push(format!(
                "  {}: `{}` = \"{}\"",
                display_manifest(&root, manifest_dir),
                escape.key,
                escape.reference
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "public-peppy-libs is a sealed tree: its crates may depend on each other \
         and on the registry, never on anything outside {root:?}. The `peppy` \
         crates depend on these, not the other way around; move what is needed \
         into the tree instead. Offending manifest entries:\n{}",
        violations.join("\n")
    );
}

#[test]
fn every_package_finds_its_workspace_root_inside_the_tree() {
    let root = tree_root();

    let adrift: Vec<String> = packages_adrift(&root, &tree_manifests(&root))
        .iter()
        .map(|manifest_dir| format!("  {}", display_manifest(&root, manifest_dir)))
        .collect();

    assert!(
        adrift.is_empty(),
        "public-peppy-libs is a sealed tree: a package here finds its workspace \
         root inside {root:?}, so that it builds the same whatever sits above \
         the tree. Cargo looks for a root in every directory above a package \
         that neither is one nor names one, and under a workspace that does not \
         list the package it stops with \"current package believes it's in a \
         workspace when it's not\". Give each of these manifests an empty \
         `[workspace]` table, or make the package a member of a workspace \
         inside the tree:\n{}",
        adrift.join("\n")
    );
}

#[test]
fn no_symlink_reaches_outside_the_tree() {
    let root = tree_root();
    let (_, symlinks) = manifests_and_symlinks(&root);

    let violations: Vec<String> = symlinks
        .iter()
        .filter_map(|link| {
            let target = std::fs::read_link(link)
                .unwrap_or_else(|error| panic!("cannot read link {link:?}: {error}"));
            let link_dir = link.parent().expect("a symlink has a parent directory");
            (!stays_inside(&root, link_dir, &target.to_string_lossy())).then(|| {
                format!(
                    "  {} -> {}",
                    link.strip_prefix(&root).unwrap_or(link).display(),
                    target.display()
                )
            })
        })
        .collect();

    assert!(
        violations.is_empty(),
        "public-peppy-libs is a sealed tree: a symlink leaving {root:?} smuggles \
         outside files into it. Offending links:\n{}",
        violations.join("\n")
    );
}

// The checks below hold the checker itself to account: a guard that cannot
// see a violation passes forever.

const ROOT: &str = "/repo/public-peppy-libs";
const CRATE_DIR: &str = "/repo/public-peppy-libs/peppy-shared/peppylib-rs";

fn escapes(manifest: &str) -> Vec<Escape> {
    escapes_in(Path::new(ROOT), Path::new(CRATE_DIR), manifest)
}

fn escape(key: &str, reference: &str) -> Escape {
    Escape {
        key: key.to_string(),
        reference: reference.to_string(),
    }
}

#[test]
fn sibling_and_registry_dependencies_are_allowed() {
    let manifest = r#"
        [package]
        name = "peppylib-rs"
        build = "build.rs"

        [lib]
        path = "src/lib.rs"

        [dependencies]
        serde = "1.0"
        config = { package = "peppy-config-model", path = "../peppy-config-model" }
        srs_model = { path = "../../srs_model" }

        [dev-dependencies]
        peppylib-rs = { path = ".", features = ["testing"] }
    "#;
    assert_eq!(escapes(manifest), vec![]);
}

#[test]
fn a_dependency_on_a_peppy_crate_is_caught_in_every_dependency_table() {
    let manifest = r#"
        [dependencies]
        auth = { path = "../../../crates/auth-internal" }

        [dev-dependencies.daemon]
        path = "../../../crates/daemon-internal"

        [build-dependencies]
        policy = { path = "../../../crates/peppylib-build-policy" }

        [target.'cfg(target_os = "linux")'.dependencies]
        containers = { path = "../../../crates/containers-internal" }

        [workspace.dependencies]
        generator = { path = "../../../crates/generator-internal" }

        [patch.crates-io]
        serde = { path = "../../../../serde" }
    "#;
    assert_eq!(
        escapes(manifest),
        vec![
            escape("dependencies.auth.path", "../../../crates/auth-internal"),
            escape(
                "dev-dependencies.daemon.path",
                "../../../crates/daemon-internal"
            ),
            escape(
                "build-dependencies.policy.path",
                "../../../crates/peppylib-build-policy"
            ),
            escape(
                "target.cfg(target_os = \"linux\").dependencies.containers.path",
                "../../../crates/containers-internal"
            ),
            escape(
                "workspace.dependencies.generator.path",
                "../../../crates/generator-internal"
            ),
            escape("patch.crates-io.serde.path", "../../../../serde"),
        ]
    );
}

#[test]
fn an_absolute_path_is_caught() {
    let manifest = r#"
        [dependencies]
        auth = { path = "/repo/crates/auth-internal" }
    "#;
    assert_eq!(
        escapes(manifest),
        vec![escape(
            "dependencies.auth.path",
            "/repo/crates/auth-internal"
        )]
    );
}

#[test]
fn outside_sources_workspaces_and_members_are_caught() {
    let manifest = r#"
        [package]
        name = "peppylib-rs"
        workspace = "../../.."
        build = "../../../crates/generator-internal/build.rs"

        [lib]
        path = "../../../crates/auth-internal/src/lib.rs"

        [[test]]
        name = "borrowed"
        path = "../../../crates/daemon-internal/tests/daemon.rs"

        [workspace]
        members = ["peppylib-rs", "../../../crates/auth-internal"]
    "#;
    assert_eq!(
        escapes(manifest),
        vec![
            escape("package.workspace", "../../.."),
            escape(
                "package.build",
                "../../../crates/generator-internal/build.rs"
            ),
            escape("lib.path", "../../../crates/auth-internal/src/lib.rs"),
            escape(
                "test.path",
                "../../../crates/daemon-internal/tests/daemon.rs"
            ),
            escape("workspace.members", "../../../crates/auth-internal"),
        ]
    );
}

fn manifest_in(dir: &str, contents: &str) -> (PathBuf, String) {
    (PathBuf::from(dir), contents.to_string())
}

#[test]
fn a_root_a_member_and_a_package_naming_its_root_are_not_adrift() {
    let manifests = [
        manifest_in(
            "/repo/public-peppy-libs/peppy-shared",
            "[workspace]\nmembers = [\"peppylib-rs\"]\n",
        ),
        manifest_in(CRATE_DIR, "[package]\nname = \"peppylib-rs\"\n"),
        manifest_in(
            "/repo/public-peppy-libs/srs_model",
            "[package]\nname = \"srs_model\"\n\n[workspace]\n",
        ),
        manifest_in(
            "/repo/public-peppy-libs/chain_kinematics",
            "[package]\nname = \"chain_kinematics\"\nworkspace = \"../peppy-shared\"\n",
        ),
    ];
    assert_eq!(
        packages_adrift(Path::new(ROOT), &manifests),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn a_package_with_no_root_inside_the_tree_is_adrift() {
    let manifests = [
        // A root above the tree is the one cargo must not be left to find.
        manifest_in("/repo", "[workspace]\nexclude = [\"public-peppy-libs\"]\n"),
        manifest_in(
            "/repo/public-peppy-libs/peppy-shared",
            "[workspace]\nmembers = [\"peppylib-rs\"]\n",
        ),
        manifest_in(
            "/repo/public-peppy-libs/srs_model",
            "[package]\nname = \"srs_model\"\n",
        ),
        // Beside a workspace root is not under it.
        manifest_in(
            "/repo/public-peppy-libs/peppy-shared-tools/lint",
            "[package]\nname = \"lint\"\n",
        ),
    ];
    assert_eq!(
        packages_adrift(Path::new(ROOT), &manifests),
        vec![
            PathBuf::from("/repo/public-peppy-libs/srs_model"),
            PathBuf::from("/repo/public-peppy-libs/peppy-shared-tools/lint"),
        ]
    );
}

#[cfg(unix)]
#[test]
fn the_walk_reports_symlinks_and_skips_tool_directories_even_when_linked() {
    use std::os::unix::fs::symlink;

    let scratch = tempfile::tempdir().expect("scratch dir");
    let root = scratch.path().join("public-peppy-libs");
    let krate = root.join("peppy-shared/json5-pretty");
    std::fs::create_dir_all(krate.join(".pixi")).expect("crate dirs");
    std::fs::write(krate.join("Cargo.toml"), "[package]\n").expect("manifest");

    // What tools leave behind: a linked-away target dir, detached pixi envs,
    // and a build tree with a manifest that is not ours.
    symlink(scratch.path(), root.join("peppy-shared/target")).expect("target link");
    symlink(scratch.path(), krate.join(".pixi/envs")).expect("pixi envs link");
    std::fs::create_dir_all(krate.join("target/package")).expect("nested target");
    std::fs::write(krate.join("target/package/Cargo.toml"), "[package]\n").expect("stray");

    // What the walk must see: a link of ours, wherever it points.
    symlink("../../../crates", krate.join("borrowed")).expect("escaping link");
    symlink("Cargo.toml", krate.join("alias.toml")).expect("inner link");

    let (manifests, symlinks) = manifests_and_symlinks(&root);
    assert_eq!(manifests, vec![krate.join("Cargo.toml")]);
    assert_eq!(
        symlinks,
        vec![krate.join("alias.toml"), krate.join("borrowed")]
    );

    assert!(stays_inside(&root, &krate, "Cargo.toml"));
    assert!(!stays_inside(&root, &krate, "../../../crates"));
}

#[test]
fn a_path_that_leaves_and_returns_is_judged_by_where_it_lands() {
    let manifest = r#"
        [dependencies]
        config = { path = "../../../public-peppy-libs/peppy-shared/peppy-config-model" }
        lookalike = { path = "../../../public-peppy-libs-fork/peppy-config-model" }
    "#;
    assert_eq!(
        escapes(manifest),
        vec![escape(
            "dependencies.lookalike.path",
            "../../../public-peppy-libs-fork/peppy-config-model"
        )]
    );
}
