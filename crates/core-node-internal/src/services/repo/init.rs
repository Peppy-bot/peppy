use crate::Result;
use crate::services::repo::{EXCLUDED_REPOS_FILE, REPOS_FILE, parse_repo_entry, source_identity};
use core_node_api::encoding::PEPPY_RELEASE_REF;
use daemon_config::consts::PeppyDirs;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use tracing::info;

const DEFAULT_REPOS_TEMPLATE: &str = include_str!("../../../assets/default_repositories.json5");

/// The file, in `conf/`, whose presence says that the default entries of
/// this `PEPPY_HOME` follow the peppy release. While it exists, peppy does
/// not change the `ref` of any entry of `repositories.json5`, so a user who
/// sets a default hub to a branch keeps that branch.
pub(crate) const DEFAULTS_FOLLOW_RELEASE_MARKER: &str = ".default_refs_follow_release";

const DEFAULTS_FOLLOW_RELEASE_MARKER_CONTENT: &str = "While this file exists, peppy does not change \
the `ref` of any entry of repositories.json5.\n";

/// The `ref` every default entry held in the template of a peppy whose
/// default hubs followed their `main` branch. An entry that is exactly such
/// a default is one this `PEPPY_HOME` never changed, and it follows the
/// release once [`follow_release_in_default_entries`] rewrites it.
const MAIN_BRANCH_REF: &str = "main";

/// Outcome of [`ensure_default_repos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// File did not exist; default template was written verbatim.
    Created,
    /// File already existed. `added` missing default entries were appended,
    /// and `now_following_release` default entries that still named `main`
    /// were set to follow the peppy release.
    Updated {
        added: usize,
        now_following_release: usize,
    },
}

/// Writes the bundled default template as `repositories.json5` in
/// `conf_dir`, verbatim so its comments and formatting are preserved, and
/// the marker that says its default entries follow the peppy release.
///
/// The one way peppy creates the file, whether `repo init`, the daemon's
/// start or the first read of the repository list gets there first.
pub(crate) fn write_default_repos(conf_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(conf_dir)?;
    std::fs::write(conf_dir.join(REPOS_FILE), DEFAULT_REPOS_TEMPLATE)?;
    write_marker(conf_dir)
}

fn write_marker(conf_dir: &Path) -> Result<()> {
    std::fs::write(
        conf_dir.join(DEFAULTS_FOLLOW_RELEASE_MARKER),
        DEFAULTS_FOLLOW_RELEASE_MARKER_CONTENT,
    )?;
    Ok(())
}

/// The default entries of the bundled template, parsed.
fn default_entries() -> Result<Vec<Value>> {
    serde_json5::from_str(DEFAULT_REPOS_TEMPLATE).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse default repositories: {e}")).into()
    })
}

/// Ensures `repositories.json5` exists and contains every entry from the
/// bundled default template.
///
/// - If the file does not exist, the default template is written verbatim
///   so its comments and formatting are preserved (see
///   [`write_default_repos`]).
/// - If the file exists and this `PEPPY_HOME` has no
///   [`DEFAULTS_FOLLOW_RELEASE_MARKER`] yet, each entry that is exactly a
///   default entry on `main` is set to follow the peppy release, along with
///   the exclusions that named it (see
///   [`follow_release_in_default_entries`]), and the marker is written. This
///   runs once per `PEPPY_HOME`.
/// - Default entries whose `id` is not already present are appended
///   verbatim. An existing entry with the same `id` claims that slot
///   regardless of its `type`, `url`, or `ref`, so users who change a
///   default's branch (or repoint it entirely) never get the default re-added
///   alongside their edit.
///
/// A file that changes is rewritten with `json5_pretty`, which drops its
/// comments.
///
/// Runs at daemon startup and is also exposed via `peppy repo init` so
/// users can resync after upgrading peppy without restarting the daemon.
pub fn ensure_default_repos(peppy_dirs: &PeppyDirs) -> Result<InitOutcome> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join(REPOS_FILE);

    let _guard = crate::services::repo::repos_file_lock().lock();

    if !repos_path.exists() {
        write_default_repos(&conf_dir)?;
        return Ok(InitOutcome::Created);
    }

    let content = std::fs::read_to_string(&repos_path)?;
    let mut existing: Vec<Value> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
    })?;
    let defaults = default_entries()?;

    let marker_exists = conf_dir.join(DEFAULTS_FOLLOW_RELEASE_MARKER).exists();
    let now_following_release = if marker_exists {
        Vec::new()
    } else {
        follow_release_in_default_entries(&mut existing, &defaults)
    };
    let added = append_missing_defaults(&mut existing, defaults);

    if added > 0 || !now_following_release.is_empty() {
        existing.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
        write_json5(&repos_path, &existing, "repositories")?;
    }
    if added > 0 {
        info!(
            "Added {} missing default repositor{} to repositories.json5",
            added,
            if added == 1 { "y" } else { "ies" }
        );
    }
    if !marker_exists {
        follow_release_in_exclusions(&conf_dir, &now_following_release)?;
        write_marker(&conf_dir)?;
    }

    Ok(InitOutcome::Updated {
        added,
        now_following_release: now_following_release.len(),
    })
}

/// Appends each default entry whose `id` no existing entry holds, and
/// returns how many it appended.
fn append_missing_defaults(existing: &mut Vec<Value>, defaults: Vec<Value>) -> usize {
    let existing_ids: HashSet<u64> = existing
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .collect();
    let missing: Vec<Value> = defaults
        .into_iter()
        .filter(|default_entry| {
            default_entry
                .get("id")
                .and_then(|v| v.as_u64())
                .is_some_and(|id| !existing_ids.contains(&id))
        })
        .collect();
    let added = missing.len();
    existing.extend(missing);
    added
}

/// A default entry as the template of a peppy whose default hubs followed
/// `main` wrote it: the entry of the current template with `ref: "main"`.
fn default_entry_on_main(default_entry: &Value) -> Value {
    let mut on_main = default_entry.clone();
    if let Some(object) = on_main.as_object_mut() {
        object.insert("ref".to_owned(), Value::String(MAIN_BRANCH_REF.to_owned()));
    }
    on_main
}

/// Replaces each entry of `existing` that is exactly a default entry on
/// `main` (the same `id`, `type`, `url`, `ref: "main"`, and no other field,
/// compared as parsed values) with that default entry, which follows the
/// peppy release. An entry that differs in any field is one the user
/// changed, and stays as it is.
///
/// Returns the replaced entries as they were, so the exclusions that named
/// them can follow.
fn follow_release_in_default_entries(existing: &mut [Value], defaults: &[Value]) -> Vec<Value> {
    let mut replaced = Vec::new();
    for entry in existing.iter_mut() {
        let Some(default_entry) = defaults
            .iter()
            .find(|default_entry| default_entry_on_main(default_entry) == *entry)
        else {
            continue;
        };
        info!(
            "repositories.json5: repository {} ({}) now follows the peppy release: its ref `{}` \
             is now `{}`",
            entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            entry.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            MAIN_BRANCH_REF,
            PEPPY_RELEASE_REF
        );
        replaced.push(std::mem::replace(entry, default_entry.clone()));
    }
    replaced
}

/// Sets each exclusion that named one of the `replaced` entries to name the
/// entry that replaced it, so a default hub the user dropped stays dropped.
/// An exclusion matches a git repository by its identity `<url>@<ref>`, and
/// the replacing entry's identity carries `@{peppy-release}` in place of
/// `main`.
fn follow_release_in_exclusions(conf_dir: &Path, replaced: &[Value]) -> Result<()> {
    let exclusions_path = conf_dir.join(EXCLUDED_REPOS_FILE);
    if replaced.is_empty() || !exclusions_path.exists() {
        return Ok(());
    }
    let replaced_identities: HashSet<String> = replaced
        .iter()
        .filter_map(|entry| parse_repo_entry(entry).ok())
        .map(|source| source_identity(&source))
        .collect();

    let content = std::fs::read_to_string(&exclusions_path)?;
    let mut exclusions: Vec<Value> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse excluded_repositories.json5: {e}"))
    })?;
    let mut changed = 0usize;
    for exclusion in exclusions.iter_mut() {
        let Some(identity) = parse_repo_entry(exclusion)
            .ok()
            .map(|source| source_identity(&source))
        else {
            continue;
        };
        let Some(object) = exclusion.as_object_mut() else {
            continue;
        };
        if !replaced_identities.contains(&identity) {
            continue;
        }
        object.insert(
            "ref".to_owned(),
            Value::String(PEPPY_RELEASE_REF.to_owned()),
        );
        info!(
            "excluded_repositories.json5: the exclusion of {identity} now names the repository \
             that follows the peppy release"
        );
        changed += 1;
    }
    if changed > 0 {
        write_json5(&exclusions_path, &exclusions, "excluded repositories")?;
    }
    Ok(())
}

fn write_json5(path: &Path, entries: &[Value], desc: &str) -> Result<()> {
    let serialized = json5_pretty::to_string_pretty(entries)
        .map_err(|e| core_node_api::Error::Encoding(format!("failed to serialize {desc}: {e}")))?;
    std::fs::write(path, serialized)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::repositories_list_path;

    /// Helper: read repositories.json5 as a Vec<Value>.
    fn read_repos(peppy_dirs: &PeppyDirs) -> Vec<Value> {
        let path = repositories_list_path(peppy_dirs);
        let content = std::fs::read_to_string(&path).unwrap();
        serde_json5::from_str(&content).unwrap()
    }

    /// Helper: returns true if any entry has the given git url.
    fn has_git_url(repos: &[Value], url: &str) -> bool {
        repos.iter().any(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("git")
                && e.get("url").and_then(|v| v.as_str()) == Some(url)
        })
    }

    #[test]
    fn creating_new_file_preserves_template_comments_and_formatting_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repos_path = repositories_list_path(&peppy_dirs);
        assert!(!repos_path.exists());

        ensure_default_repos(&peppy_dirs).unwrap();

        assert!(repos_path.exists());
        // Verbatim write preserves comments from the json5 template.
        let written = std::fs::read_to_string(&repos_path).unwrap();
        assert_eq!(written, DEFAULT_REPOS_TEMPLATE);
        assert!(
            read_repos(&peppy_dirs)
                .iter()
                .all(|entry| entry.get("ref").and_then(|v| v.as_str()) == Some("@{peppy-release}")),
            "every default follows the peppy release"
        );
        assert!(marker_exists(&peppy_dirs));
    }

    fn marker_exists(peppy_dirs: &PeppyDirs) -> bool {
        peppy_dirs
            .conf_dir()
            .join(DEFAULTS_FOLLOW_RELEASE_MARKER)
            .exists()
    }

    /// The default entries as the template of a peppy whose default hubs
    /// followed `main` wrote them.
    fn old_default_entries() -> Vec<Value> {
        default_entries()
            .unwrap()
            .iter()
            .map(default_entry_on_main)
            .collect()
    }

    fn write_repos(peppy_dirs: &PeppyDirs, entries: &[Value]) {
        std::fs::create_dir_all(peppy_dirs.conf_dir()).unwrap();
        std::fs::write(
            repositories_list_path(peppy_dirs),
            serde_json::to_string_pretty(entries).unwrap(),
        )
        .unwrap();
    }

    fn entry_with_id(repos: &[Value], id: u64) -> &Value {
        repos
            .iter()
            .find(|e| e.get("id").and_then(|v| v.as_u64()) == Some(id))
            .unwrap_or_else(|| panic!("an entry with id {id}: {repos:?}"))
    }

    /// Each default entry an older template wrote on `main`, and that the
    /// user never changed, follows the release after the first start. An
    /// entry the user changed in any field, and every other entry, stays as
    /// it is, and a second run changes nothing at all.
    #[test]
    fn old_default_entries_follow_the_release_once() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let mut entries = old_default_entries();
        // 1003: the user moved it to another branch.
        entries[3]["ref"] = Value::String("dev".to_owned());
        // 1004: the user added a field of their own.
        entries[4]["note"] = Value::String("mine".to_owned());
        let private_hub = serde_json::json!({
            "id": 2000,
            "type": "git",
            "url": "git@github.com:Peppy-bot/private-nodes-hub.git",
            "ref": "main",
        });
        entries.push(private_hub.clone());
        write_repos(&peppy_dirs, &entries);
        assert!(!marker_exists(&peppy_dirs));

        let outcome = ensure_default_repos(&peppy_dirs).unwrap();

        assert_eq!(
            outcome,
            InitOutcome::Updated {
                added: 0,
                now_following_release: 3
            }
        );
        let repos = read_repos(&peppy_dirs);
        let defaults = default_entries().unwrap();
        for id in [1000, 1001, 1002] {
            let template_entry = defaults
                .iter()
                .find(|e| e.get("id").and_then(|v| v.as_u64()) == Some(id))
                .unwrap();
            assert_eq!(entry_with_id(&repos, id), template_entry);
        }
        assert_eq!(entry_with_id(&repos, 1003), &entries[3]);
        assert_eq!(entry_with_id(&repos, 1004), &entries[4]);
        assert_eq!(entry_with_id(&repos, 2000), &private_hub);
        assert!(marker_exists(&peppy_dirs));

        let before = std::fs::read_to_string(repositories_list_path(&peppy_dirs)).unwrap();
        let second = ensure_default_repos(&peppy_dirs).unwrap();
        assert_eq!(
            second,
            InitOutcome::Updated {
                added: 0,
                now_following_release: 0
            }
        );
        let after = std::fs::read_to_string(repositories_list_path(&peppy_dirs)).unwrap();
        assert_eq!(before, after, "a second run leaves the file byte for byte");
    }

    /// Once the marker exists, a default the user sets back to `main` stays
    /// on `main`: the rewrite runs once per `PEPPY_HOME`.
    #[test]
    fn a_default_the_user_sets_to_main_later_stays_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        ensure_default_repos(&peppy_dirs).unwrap();
        write_repos(&peppy_dirs, &old_default_entries());
        let before = std::fs::read_to_string(repositories_list_path(&peppy_dirs)).unwrap();

        let outcome = ensure_default_repos(&peppy_dirs).unwrap();

        assert_eq!(
            outcome,
            InitOutcome::Updated {
                added: 0,
                now_following_release: 0
            }
        );
        let after = std::fs::read_to_string(repositories_list_path(&peppy_dirs)).unwrap();
        assert_eq!(before, after);
    }

    /// A default hub the user excluded while it followed `main` stays
    /// excluded once it follows the release: the exclusion names it by its
    /// identity, which the rewrite changes. An exclusion of another
    /// repository is left alone.
    #[test]
    fn an_excluded_default_stays_excluded_when_it_follows_the_release() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_repos(&peppy_dirs, &old_default_entries());
        let other = serde_json::json!({
            "id": 2, "type": "git", "url": "https://example.com/other.git", "ref": "main",
        });
        std::fs::write(
            peppy_dirs.conf_dir().join(EXCLUDED_REPOS_FILE),
            serde_json::to_string_pretty(&serde_json::json!([
                {
                    "id": 1,
                    "type": "git",
                    "url": "https://github.com/Peppy-bot/nodes-hub.git",
                    "ref": "main",
                },
                other,
            ]))
            .unwrap(),
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let exclusions = crate::services::repo::exclude::ExclusionSet::load(&peppy_dirs);
        assert!(
            exclusions.is_excluded("https://github.com/Peppy-bot/nodes-hub.git@@{peppy-release}")
        );
        assert!(!exclusions.is_excluded("https://github.com/Peppy-bot/nodes-hub.git@main"));
        assert!(exclusions.is_excluded("https://example.com/other.git@main"));
    }

    /// A user upgrades peppy and a new entry (`launchers-hub`) is added
    /// to the bundled defaults, but their pre-existing `repositories.json5`
    /// only contains the older entries.
    /// `ensure_default_repos` must add the missing default(s) without
    /// disturbing what is already there.
    #[test]
    fn adds_missing_default_when_file_already_has_some_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(
            &repos_path,
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes-hub.git", "ref": "main" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        assert!(
            has_git_url(&repos, "https://github.com/Peppy-bot/nodes-hub.git"),
            "pre-existing nodes-hub entry must be preserved, got: {repos:?}"
        );
        assert!(
            has_git_url(&repos, "https://github.com/Peppy-bot/launchers-hub.git"),
            "missing launchers-hub default must be appended, got: {repos:?}"
        );
    }

    #[test]
    fn preserves_user_repos_when_adding_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1, "type": "fs", "path": "/home/me/my_nodes" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        assert!(
            repos
                .iter()
                .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("fs")
                    && e.get("path").and_then(|v| v.as_str()) == Some("/home/me/my_nodes")),
            "user fs repo must be preserved"
        );
        assert!(has_git_url(
            &repos,
            "https://github.com/Peppy-bot/nodes-hub.git"
        ));
        assert!(has_git_url(
            &repos,
            "https://github.com/Peppy-bot/launchers-hub.git"
        ));
    }

    #[test]
    fn no_changes_when_all_defaults_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(&repos_path, DEFAULT_REPOS_TEMPLATE).unwrap();
        let content_before = std::fs::read_to_string(&repos_path).unwrap();

        let outcome = ensure_default_repos(&peppy_dirs).unwrap();

        // Nothing needs adding when the file already holds every default, and
        // the file is left byte-for-byte unchanged. Asserted directly on the
        // outcome + content (deterministic) rather than via filesystem mtime +
        // a sleep, which depended on wall-clock granularity.
        assert_eq!(
            outcome,
            InitOutcome::Updated {
                added: 0,
                now_following_release: 0
            }
        );
        let content_after = std::fs::read_to_string(&repos_path).unwrap();
        assert_eq!(
            content_before, content_after,
            "file should not be rewritten when nothing changes"
        );
    }

    #[test]
    fn is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes-hub.git", "ref": "main" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();
        let after_first = read_repos(&peppy_dirs);
        ensure_default_repos(&peppy_dirs).unwrap();
        let after_second = read_repos(&peppy_dirs);

        assert_eq!(
            after_first, after_second,
            "second invocation must not introduce duplicates"
        );
    }

    /// If an existing entry occupies a default's id, that id is considered
    /// taken and the default is NOT re-added, regardless of url/ref/type.
    /// This is what stops the daemon from duplicating defaults when a user
    /// edits a default entry's ref (e.g. main → feature/x).
    #[test]
    fn skips_default_when_id_is_already_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes-hub.git", "ref": "feature/v0.10.0" },
                { "id": 1001, "type": "fs", "path": "/some/where" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        // Exactly one entry per id 1000 and 1001: the user's entries, not the defaults.
        let id_1000: Vec<_> = repos
            .iter()
            .filter(|e| e.get("id").and_then(|v| v.as_u64()) == Some(1000))
            .collect();
        assert_eq!(
            id_1000.len(),
            1,
            "id 1000 must not be duplicated: {repos:?}"
        );
        assert_eq!(
            id_1000[0].get("ref").and_then(|v| v.as_str()),
            Some("feature/v0.10.0"),
            "user's edited ref must be preserved"
        );

        let id_1001: Vec<_> = repos
            .iter()
            .filter(|e| e.get("id").and_then(|v| v.as_u64()) == Some(1001))
            .collect();
        assert_eq!(
            id_1001.len(),
            1,
            "id 1001 must not be duplicated: {repos:?}"
        );
        assert_eq!(
            id_1001[0].get("type").and_then(|v| v.as_str()),
            Some("fs"),
            "user's fs entry at id 1001 must be preserved"
        );

        // launchers-hub default is not present; id 1001 is taken, so it is skipped.
        assert!(
            !has_git_url(&repos, "https://github.com/Peppy-bot/launchers-hub.git"),
            "launchers-hub default must not be added when its id 1001 is taken"
        );
    }

    /// `repo remove` deletes an entry by id. Subsequent init runs would
    /// re-add it (this is a deliberate trade-off; the user can use
    /// `repo exclude` for permanent suppression). Cover the read-back to
    /// document the behaviour.
    #[test]
    fn re_adds_default_after_repo_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        ensure_default_repos(&peppy_dirs).unwrap();
        let initial = read_repos(&peppy_dirs);
        assert!(has_git_url(
            &initial,
            "https://github.com/Peppy-bot/launchers-hub.git"
        ));

        // Simulate `repo remove` deleting the launchers-hub entry.
        let mut without_launchers: Vec<Value> = initial
            .into_iter()
            .filter(|e| {
                e.get("url").and_then(|v| v.as_str())
                    != Some("https://github.com/Peppy-bot/launchers-hub.git")
            })
            .collect();
        let serialized = json5_pretty::to_string_pretty(&without_launchers).unwrap();
        std::fs::write(repositories_list_path(&peppy_dirs), &serialized).unwrap();
        without_launchers.clear(); // dropped, only used to write

        ensure_default_repos(&peppy_dirs).unwrap();

        let after = read_repos(&peppy_dirs);
        assert!(
            has_git_url(&after, "https://github.com/Peppy-bot/launchers-hub.git"),
            "init should re-add a removed default"
        );
    }
}
