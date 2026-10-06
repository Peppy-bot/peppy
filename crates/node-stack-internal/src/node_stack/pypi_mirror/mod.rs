//! The PyPI mirror of Python node builds (`pypi_mirror` in
//! `peppy_config.json5`).
//!
//! Before a Python node builds, [`apply`] points each PyPI file that a
//! staged `uv.lock` pins at the mirror, when the mirror has it:
//!
//! 1. [`lock`] parses each `uv.lock` of the staged tree and collects the
//!    files it pins by sha256 for a registry package at a URL below
//!    `https://files.pythonhosted.org/packages/`.
//! 2. [`presence`] asks the mirror for each of those files with a `HEAD`
//!    request. A mirror can lag behind PyPI, and a file it does not have
//!    stays on PyPI.
//! 3. [`lock`] puts the mirror base in the URL of each file the mirror has,
//!    and the staged lock is replaced through a temporary file and a rename.
//!
//! uv checks every file it downloads against the sha256 of the lock, which
//! comes from the repository, so a mirror can make a build slow or fail but
//! cannot change what the build installs. uv accepts the rewritten lock with
//! `uv sync`, `--locked` and `--frozen`, as only the URLs of registry files
//! change. Every step fails open: an error leaves the lock it concerns as it
//! is, shows a warning line, and the build downloads those files from PyPI.
//!
//! The staged tree is fingerprinted before the rewrite, so a build finds the
//! same artifact with and without a mirror. A build that fails puts the
//! original locks back ([`RewrittenLocks::restore`]): a cancelled build hands
//! its staged tree to the next build of the node, which fingerprints it
//! again.

mod lock;
mod presence;
#[cfg(test)]
mod test_support;

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon_config::peppy_config::PackagesBaseUrl;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::action_log::Announcer;
use crate::build_progress::BUILD_PROGRESS_SAMPLE_INTERVAL;
use lock::{PYPI_FILES_HOST, PypiFile, UvLock};

/// The name of the lock files the mirror rewrites.
const UV_LOCK_FILE_NAME: &str = "uv.lock";

/// The start of every line of the mirror step, so the user can tell them
/// from the lines of the build.
const LINE_PREFIX: &str = "PyPI mirror: ";

/// The shortest time between two lines that tell how far the presence check
/// got. The check sends no line until it is done, and the build idle clock
/// resets only on a line, so a long check on a slow mirror reports its
/// progress at the cadence of the activity lines of the build itself.
const PROGRESS_LINE_INTERVAL: Duration = BUILD_PROGRESS_SAMPLE_INTERVAL;

/// Where the lines of the mirror step go: the build log and the build
/// feedback channel, each line after [`LINE_PREFIX`].
pub(super) struct BuildFeedback {
    announcer: Announcer,
}

impl BuildFeedback {
    pub(super) fn new(announcer: Announcer) -> Self {
        Self { announcer }
    }

    fn line(&self, line: String) {
        self.announcer.line(format!("{LINE_PREFIX}{line}"));
    }

    fn warning(&self, line: String) {
        self.announcer.warning(format!("{LINE_PREFIX}{line}"));
    }
}

/// A warning about a problem after which no file comes from the mirror.
fn every_file_from_pypi(problem: String) -> String {
    format!("{problem}; every locked file comes from {PYPI_FILES_HOST}")
}

/// A warning about a problem with one lock, whose files then come from PyPI.
fn its_files_from_pypi(problem: String) -> String {
    format!("{problem}; its files come from {PYPI_FILES_HOST}")
}

/// Points the PyPI files of the `uv.lock` files in `working_dir` at
/// `mirror`, for each file the mirror has, and ends with one line per lock
/// that says how many of its files come from the mirror. Returns the locks
/// it rewrote, which a build that fails puts back with
/// [`RewrittenLocks::restore`].
///
/// Never fails the build: an error is a warning line, and the files it
/// concerns come from PyPI. A build that `cancel_token` cancels during the
/// presence check stops the step with no further line, and its staged locks
/// stay as they are.
pub(super) async fn apply(
    working_dir: &Path,
    mirror: &PackagesBaseUrl,
    feedback: &BuildFeedback,
    cancel_token: &CancellationToken,
) -> RewrittenLocks {
    let read = {
        let working_dir = working_dir.to_path_buf();
        tokio::task::spawn_blocking(move || read_staged_locks(&working_dir)).await
    };
    let locks = match read {
        Ok(read) => {
            for warning in read.warnings {
                feedback.warning(warning);
            }
            read.locks
        }
        Err(e) => {
            feedback.warning(every_file_from_pypi(format!(
                "the uv.lock files could not be read ({e})"
            )));
            Vec::new()
        }
    };
    let mut outcomes: Vec<LockOutcome> = locks
        .iter()
        .map(|staged| LockOutcome {
            shown_as: staged.shown_as.clone(),
            locked_files: staged.lock.pypi_files().len(),
            redirected_files: 0,
        })
        .collect();

    let Some(on_mirror) = files_on_mirror(&locks, mirror, feedback, cancel_token).await else {
        return RewrittenLocks::none();
    };

    let mut rewritten = RewrittenLocks::none();
    if !on_mirror.is_empty() {
        let mirror = mirror.clone();
        let written =
            tokio::task::spawn_blocking(move || write_redirected_locks(locks, &mirror, &on_mirror))
                .await;
        match written {
            Ok(written) => {
                for warning in written.warnings {
                    feedback.warning(warning);
                }
                for (outcome, redirected_files) in outcomes.iter_mut().zip(written.redirected_files)
                {
                    outcome.redirected_files = redirected_files;
                }
                rewritten = written.rewritten;
            }
            Err(e) => feedback.warning(every_file_from_pypi(format!(
                "the uv.lock files could not be written ({e})"
            ))),
        }
    }

    if outcomes.is_empty() {
        feedback.line(format!(
            "{}; no readable uv.lock in the node",
            mirror.host()
        ));
    }
    for outcome in &outcomes {
        feedback.line(outcome.summary(mirror));
    }
    rewritten
}

/// How many PyPI files of one staged lock point at the mirror after the
/// step.
struct LockOutcome {
    shown_as: String,
    locked_files: usize,
    redirected_files: usize,
}

impl LockOutcome {
    /// The line that names the lock, so a lock that the build does not read
    /// (the lock of a test project, say) cannot pass for the lock it does.
    fn summary(&self, mirror: &PackagesBaseUrl) -> String {
        format!(
            "{}; {}: {} of {} locked files from the mirror, {} from {PYPI_FILES_HOST}",
            mirror.host(),
            self.shown_as,
            self.redirected_files,
            self.locked_files,
            self.locked_files - self.redirected_files
        )
    }
}

/// The staged locks that [`apply`] rewrote, with the text each one had when
/// it was staged.
pub(super) struct RewrittenLocks {
    originals: Vec<OriginalLock>,
}

/// The text a rewritten lock had when it was staged.
struct OriginalLock {
    path: PathBuf,
    shown_as: String,
    text: String,
}

impl RewrittenLocks {
    /// No rewritten lock: the build has no mirror, or the step changed
    /// nothing.
    pub(super) fn none() -> Self {
        Self {
            originals: Vec::new(),
        }
    }

    /// Puts back the text each rewritten lock had when it was staged.
    /// A failure is a warning line.
    pub(super) async fn restore(self, feedback: &BuildFeedback) {
        if self.originals.is_empty() {
            return;
        }
        let restored = tokio::task::spawn_blocking(move || restore_originals(self.originals)).await;
        match restored {
            Ok(warnings) => {
                for warning in warnings {
                    feedback.warning(warning);
                }
            }
            Err(e) => feedback.warning(format!(
                "the staged uv.lock files could not be put back ({e})"
            )),
        }
    }
}

fn restore_originals(originals: Vec<OriginalLock>) -> Vec<String> {
    originals
        .into_iter()
        .filter_map(|original| {
            replace_file(&original.path, &original.text)
                .err()
                .map(|e| format!("cannot put back the staged {} ({e})", original.shown_as))
        })
        .collect()
}

/// A `uv.lock` of the staged tree.
struct StagedLock {
    path: PathBuf,
    /// The path relative to the staged tree, which the feedback names.
    shown_as: String,
    /// The text of the file as it was staged.
    text: String,
    lock: UvLock,
}

/// The `uv.lock` files of the staged tree that parse, in path order, and a
/// warning for each one that does not.
#[derive(Default)]
struct StagedLocks {
    locks: Vec<StagedLock>,
    warnings: Vec<String>,
}

fn read_staged_locks(working_dir: &Path) -> StagedLocks {
    let mut read = StagedLocks::default();
    let paths = match find_uv_locks(working_dir) {
        Ok(paths) => paths,
        Err(e) => {
            read.warnings.push(every_file_from_pypi(format!(
                "cannot search the staged node for uv.lock files ({e})"
            )));
            return read;
        }
    };
    for path in paths {
        let shown_as = path
            .strip_prefix(working_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| match UvLock::parse(&text) {
                Ok(lock) => Ok((text, lock)),
                Err(e) => Err(parse_error(&text, &e)),
            });
        match parsed {
            Ok((text, lock)) => read.locks.push(StagedLock {
                path,
                shown_as,
                text,
                lock,
            }),
            Err(reason) => read.warnings.push(its_files_from_pypi(format!(
                "cannot read {shown_as} ({reason})"
            ))),
        }
    }
    read
}

/// One line for a TOML error: its message and the line it is on. The
/// `Display` of the error spans several lines, with the source text.
fn parse_error(text: &str, error: &toml_edit::TomlError) -> String {
    let message = error.message().trim();
    match error.span() {
        Some(span) => {
            let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
            format!("line {line}: {message}")
        }
        None => message.to_string(),
    }
}

/// Every regular file named `uv.lock` in the staged tree, in path order.
/// Symlinks are not followed, so the step reads and writes inside the staged
/// tree only.
fn find_uv_locks(working_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut dirs = vec![working_dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                dirs.push(entry.path());
            } else if file_type.is_file() && entry.file_name() == UV_LOCK_FILE_NAME {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The PyPI files of `locks` that `mirror` has, or `None` when
/// `cancel_token` cancels the build during the check. A check that cannot
/// run is a warning, and no file comes from the mirror.
async fn files_on_mirror(
    locks: &[StagedLock],
    mirror: &PackagesBaseUrl,
    feedback: &BuildFeedback,
    cancel_token: &CancellationToken,
) -> Option<HashSet<PypiFile>> {
    // Two locks can pin the same file; the mirror is asked once for it.
    let files: Vec<PypiFile> = locks
        .iter()
        .flat_map(|staged| staged.lock.pypi_files())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if files.is_empty() {
        return Some(HashSet::new());
    }
    let client = match presence::client() {
        Ok(client) => client,
        Err(e) => {
            feedback.warning(every_file_from_pypi(format!(
                "cannot create the HTTP client ({e})"
            )));
            return Some(HashSet::new());
        }
    };

    let mut progress = ProgressThrottle::new(Instant::now());
    let check = presence::files_on_mirror(&client, mirror, &files, |checked| {
        if progress.line_due(Instant::now()) {
            feedback.line(format!(
                "checked {checked} of {} distinct locked files on {}",
                files.len(),
                mirror.host()
            ));
        }
    });
    let result = tokio::select! {
        result = check => result,
        () = cancel_token.cancelled() => return None,
    };
    match result {
        Ok(on_mirror) => Some(on_mirror),
        Err(unreachable) => {
            feedback.warning(every_file_from_pypi(format!(
                "{} does not answer ({unreachable})",
                mirror.host()
            )));
            Some(HashSet::new())
        }
    }
}

/// Lets through at most one progress line per [`PROGRESS_LINE_INTERVAL`].
struct ProgressThrottle {
    last_line: Instant,
}

impl ProgressThrottle {
    fn new(started: Instant) -> Self {
        Self { last_line: started }
    }

    /// Whether a progress line is due at `now`. When it is, the next one is
    /// due one interval later.
    fn line_due(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_line) < PROGRESS_LINE_INTERVAL {
            return false;
        }
        self.last_line = now;
        true
    }
}

/// For each lock, in order, how many of its file entries point at the mirror
/// after the write; the locks that were rewritten; and a warning for each
/// lock that could not be written.
struct WrittenLocks {
    redirected_files: Vec<usize>,
    rewritten: RewrittenLocks,
    warnings: Vec<String>,
}

fn write_redirected_locks(
    locks: Vec<StagedLock>,
    mirror: &PackagesBaseUrl,
    on_mirror: &HashSet<PypiFile>,
) -> WrittenLocks {
    let mut written = WrittenLocks {
        redirected_files: Vec::with_capacity(locks.len()),
        rewritten: RewrittenLocks::none(),
        warnings: Vec::new(),
    };
    for staged in locks {
        let redirected = staged.lock.redirect(mirror, on_mirror);
        if redirected.redirected_files == 0 {
            written.redirected_files.push(0);
            continue;
        }
        match replace_file(&staged.path, &redirected.text) {
            Ok(()) => {
                written.redirected_files.push(redirected.redirected_files);
                written.rewritten.originals.push(OriginalLock {
                    path: staged.path,
                    shown_as: staged.shown_as,
                    text: staged.text,
                });
            }
            Err(e) => {
                written.redirected_files.push(0);
                written.warnings.push(its_files_from_pypi(format!(
                    "cannot write {} ({e})",
                    staged.shown_as
                )));
            }
        }
    }
    written
}

/// Replaces the file at `path` with `text` through a temporary file in the
/// same directory and a rename, so a build never reads half a lock, and
/// keeps the permissions of the file.
fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    let permissions = std::fs::metadata(path)?.permissions();
    daemon_config::atomic_write::publish_atomic(path, |tmp| {
        std::fs::write(tmp, text)?;
        std::fs::set_permissions(tmp, permissions)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_log::test_support::scratch_log;
    use crate::build_io::{FeedbackLine, FeedbackStream};
    use httpmock::Method::HEAD;
    use httpmock::MockServer;
    use test_support::{closing_mirror, lock_with_wheels, mirror_of, silent_mirror, wheel_paths};
    use tokio::sync::mpsc;

    /// The feedback of one run of the step: each line with its stream.
    struct CapturedFeedback {
        announcer: Announcer,
        feedback_rx: mpsc::UnboundedReceiver<FeedbackLine>,
    }

    impl CapturedFeedback {
        fn new() -> Self {
            let (feedback_tx, feedback_rx) = mpsc::unbounded_channel();
            Self {
                announcer: Announcer::new(scratch_log(), feedback_tx),
                feedback_rx,
            }
        }

        fn feedback(&self) -> BuildFeedback {
            BuildFeedback::new(self.announcer.clone())
        }

        /// Every line sent since the last call: the warnings, then the other
        /// lines, each in the order they were sent.
        fn drain(&mut self) -> (Vec<String>, Vec<String>) {
            let mut warnings = Vec::new();
            let mut lines = Vec::new();
            while let Ok(line) = self.feedback_rx.try_recv() {
                match line.stream {
                    FeedbackStream::Warning => warnings.push(line.line),
                    _ => lines.push(line.line),
                }
            }
            (warnings, lines)
        }
    }

    #[test]
    fn a_progress_line_is_due_once_per_interval() {
        let started = Instant::now();
        let mut throttle = ProgressThrottle::new(started);

        assert!(!throttle.line_due(started));
        assert!(!throttle.line_due(started + PROGRESS_LINE_INTERVAL - Duration::from_millis(1)));
        assert!(throttle.line_due(started + PROGRESS_LINE_INTERVAL));
        assert!(
            !throttle.line_due(started + PROGRESS_LINE_INTERVAL + Duration::from_millis(1)),
            "the next line waits for one more interval"
        );
        assert!(throttle.line_due(started + PROGRESS_LINE_INTERVAL * 2));
    }

    #[test]
    fn the_summary_names_the_lock_and_counts_the_files_from_each_host() {
        let mirror = PackagesBaseUrl::parse("https://pypi.tuna.tsinghua.edu.cn/packages/").unwrap();
        let outcome = LockOutcome {
            shown_as: "uv.lock".to_string(),
            locked_files: 870,
            redirected_files: 860,
        };
        assert_eq!(
            outcome.summary(&mirror),
            "pypi.tuna.tsinghua.edu.cn; uv.lock: 860 of 870 locked files from the mirror, \
             10 from files.pythonhosted.org"
        );
    }

    #[test]
    fn every_line_of_the_step_starts_with_the_prefix() {
        let mut captured = CapturedFeedback::new();

        captured.feedback().line("a line".to_string());
        captured.feedback().warning("a warning".to_string());

        assert_eq!(
            captured.drain(),
            (
                vec!["PyPI mirror: a warning".to_string()],
                vec!["PyPI mirror: a line".to_string()]
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn uv_locks_are_found_in_the_whole_tree_without_following_symlinks() {
        let tree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join("sub/deeper")).unwrap();
        std::fs::write(tree.path().join("uv.lock"), "").unwrap();
        std::fs::write(tree.path().join("sub/deeper/uv.lock"), "").unwrap();
        std::fs::write(tree.path().join("sub/not-uv.lock"), "").unwrap();
        std::fs::create_dir_all(outside.path().join("linked")).unwrap();
        std::fs::write(outside.path().join("linked/uv.lock"), "").unwrap();
        std::os::unix::fs::symlink(outside.path().join("linked"), tree.path().join("link-dir"))
            .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("linked/uv.lock"),
            tree.path().join("sub/uv.lock"),
        )
        .unwrap();

        assert_eq!(
            find_uv_locks(tree.path()).unwrap(),
            [
                tree.path().join("sub/deeper/uv.lock"),
                tree.path().join("uv.lock")
            ]
        );
    }

    #[test]
    fn a_lock_that_does_not_parse_is_named_with_its_line() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join("app")).unwrap();
        std::fs::write(
            tree.path().join("app/uv.lock"),
            "version = 1\n\n[[package]\nname = \"a\"\n",
        )
        .unwrap();

        let read = read_staged_locks(tree.path());

        assert!(read.locks.is_empty());
        assert_eq!(read.warnings.len(), 1);
        let warning = &read.warnings[0];
        assert!(
            warning.starts_with("cannot read app/uv.lock (line 3: "),
            "{warning}"
        );
        assert!(
            warning.ends_with("; its files come from files.pythonhosted.org"),
            "{warning}"
        );
        assert!(!warning.contains('\n'), "{warning}");
    }

    #[cfg(unix)]
    #[test]
    fn a_rewritten_lock_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tree = tempfile::tempdir().unwrap();
        let path = tree.path().join("uv.lock");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        replace_file(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(
            std::fs::read_dir(tree.path()).unwrap().count(),
            1,
            "no temporary file stays behind"
        );
    }

    /// Several locks: one line per lock in path order, one request per
    /// distinct file, a warning for a lock that does not parse, and a
    /// restore that puts back exactly the locks the step rewrote.
    #[tokio::test]
    async fn each_lock_gets_its_line_and_a_shared_file_is_asked_for_once() {
        let server = MockServer::start_async().await;
        let shared = server
            .mock_async(|when, then| {
                when.method(HEAD).path("/packages/aa/shared.whl");
                then.status(200).header("content-length", "40");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(HEAD).path("/packages/aa/root_only.whl");
                then.status(200).header("content-length", "40");
            })
            .await;
        server
            .mock_async(|when, then| {
                when.any_request();
                then.status(404);
            })
            .await;
        let mirror = mirror_of(&server);
        let tree = tempfile::tempdir().unwrap();
        let root_lock = lock_with_wheels(&[
            ("aa/shared.whl", Some(40)),
            ("aa/root_only.whl", Some(40)),
            ("aa/absent.whl", Some(40)),
        ]);
        let tests_lock = lock_with_wheels(&[("aa/shared.whl", Some(40))]);
        std::fs::write(tree.path().join("uv.lock"), &root_lock).unwrap();
        std::fs::create_dir_all(tree.path().join("tests")).unwrap();
        std::fs::write(tree.path().join("tests/uv.lock"), &tests_lock).unwrap();
        std::fs::create_dir_all(tree.path().join("broken")).unwrap();
        std::fs::write(tree.path().join("broken/uv.lock"), "[[package]\n").unwrap();
        let mut captured = CapturedFeedback::new();

        let rewritten = apply(
            tree.path(),
            &mirror,
            &captured.feedback(),
            &CancellationToken::new(),
        )
        .await;

        let (warnings, lines) = captured.drain();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("PyPI mirror: cannot read broken/uv.lock (line 1: "),
            "{warnings:?}"
        );
        let host = mirror.host();
        assert_eq!(
            lines,
            [
                format!(
                    "PyPI mirror: {host}; tests/uv.lock: 1 of 1 locked files from the mirror, \
                     0 from files.pythonhosted.org"
                ),
                format!(
                    "PyPI mirror: {host}; uv.lock: 2 of 3 locked files from the mirror, \
                     1 from files.pythonhosted.org"
                ),
            ]
        );
        assert_eq!(shared.calls_async().await, 1);
        let mirror_url = |path: &str| format!("{}{path}", mirror.as_str());
        let on_pypi = |path: &str| format!("https://files.pythonhosted.org/packages/{path}");
        assert_eq!(
            std::fs::read_to_string(tree.path().join("uv.lock")).unwrap(),
            root_lock
                .replacen(&on_pypi("aa/shared.whl"), &mirror_url("aa/shared.whl"), 1)
                .replacen(
                    &on_pypi("aa/root_only.whl"),
                    &mirror_url("aa/root_only.whl"),
                    1
                )
        );

        rewritten.restore(&captured.feedback()).await;

        assert_eq!(
            std::fs::read_to_string(tree.path().join("uv.lock")).unwrap(),
            root_lock
        );
        assert_eq!(
            std::fs::read_to_string(tree.path().join("tests/uv.lock")).unwrap(),
            tests_lock
        );
        assert_eq!(captured.drain(), (Vec::new(), Vec::new()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_lock_that_cannot_be_written_keeps_its_files_on_pypi() {
        use std::os::unix::fs::PermissionsExt;

        if nix::unistd::geteuid().is_root() {
            // Root writes into a read-only directory; the failure cannot be
            // made.
            return;
        }
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(HEAD);
                then.status(200).header("content-length", "40");
            })
            .await;
        let mirror = mirror_of(&server);
        let tree = tempfile::tempdir().unwrap();
        let lock = lock_with_wheels(&[("aa/a.whl", Some(40))]);
        let read_only = tree.path().join("read_only");
        std::fs::create_dir_all(&read_only).unwrap();
        std::fs::write(read_only.join("uv.lock"), &lock).unwrap();
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut captured = CapturedFeedback::new();

        let rewritten = apply(
            tree.path(),
            &mirror,
            &captured.feedback(),
            &CancellationToken::new(),
        )
        .await;

        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (warnings, lines) = captured.drain();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("PyPI mirror: cannot write read_only/uv.lock (")
                && warnings[0].ends_with("; its files come from files.pythonhosted.org"),
            "{warnings:?}"
        );
        assert_eq!(
            lines,
            [format!(
                "PyPI mirror: {}; read_only/uv.lock: 0 of 1 locked files from the mirror, \
                 1 from files.pythonhosted.org",
                mirror.host()
            )]
        );
        assert_eq!(
            std::fs::read_to_string(read_only.join("uv.lock")).unwrap(),
            lock
        );
        assert!(rewritten.originals.is_empty());
    }

    #[tokio::test]
    async fn a_mirror_that_does_not_answer_leaves_every_file_on_pypi() {
        let mirror = closing_mirror().await;
        let tree = tempfile::tempdir().unwrap();
        let paths = wheel_paths(presence::UNANSWERED_REQUESTS_TO_STOP * 2);
        let wheels: Vec<(&str, Option<u64>)> =
            paths.iter().map(|path| (path.as_str(), Some(40))).collect();
        let lock = lock_with_wheels(&wheels);
        std::fs::write(tree.path().join("uv.lock"), &lock).unwrap();
        let mut captured = CapturedFeedback::new();

        apply(
            tree.path(),
            &mirror,
            &captured.feedback(),
            &CancellationToken::new(),
        )
        .await;

        let (warnings, lines) = captured.drain();
        let host = mirror.host();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with(&format!("PyPI mirror: {host} does not answer ("))
                && warnings[0].ends_with("; every locked file comes from files.pythonhosted.org"),
            "{warnings:?}"
        );
        let locked = paths.len();
        assert_eq!(
            lines,
            [format!(
                "PyPI mirror: {host}; uv.lock: 0 of {locked} locked files from the mirror, \
                 {locked} from files.pythonhosted.org"
            )]
        );
        assert_eq!(
            std::fs::read_to_string(tree.path().join("uv.lock")).unwrap(),
            lock
        );
    }

    #[tokio::test]
    async fn a_cancelled_build_stops_the_check_with_no_line_and_no_rewrite() {
        let mirror = silent_mirror().await;
        let tree = tempfile::tempdir().unwrap();
        let lock = lock_with_wheels(&[("aa/a.whl", Some(40))]);
        std::fs::write(tree.path().join("uv.lock"), &lock).unwrap();
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let mut captured = CapturedFeedback::new();

        let rewritten = apply(tree.path(), &mirror, &captured.feedback(), &cancel_token).await;

        assert!(rewritten.originals.is_empty());
        assert_eq!(captured.drain(), (Vec::new(), Vec::new()));
        assert_eq!(
            std::fs::read_to_string(tree.path().join("uv.lock")).unwrap(),
            lock
        );
    }

    #[tokio::test]
    async fn a_node_with_no_uv_lock_says_so() {
        let tree = tempfile::tempdir().unwrap();
        let mirror = PackagesBaseUrl::parse("https://pypi.tuna.tsinghua.edu.cn/packages/").unwrap();
        let mut captured = CapturedFeedback::new();

        apply(
            tree.path(),
            &mirror,
            &captured.feedback(),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            captured.drain(),
            (
                Vec::new(),
                vec![
                    "PyPI mirror: pypi.tuna.tsinghua.edu.cn; no readable uv.lock in the node"
                        .to_string()
                ]
            )
        );
    }
}
