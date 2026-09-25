//! A `git daemon` that serves test repositories over `git://`.
//!
//! libgit2 reads a path or a `file://` URL through its local transport, and
//! [`super::git_utils::clone_repo_shallow`] clones in full there because that
//! transport refuses a shallow fetch. A full clone holds every commit and
//! every tag of the source, so a test of what a shallow clone lacks would
//! pass without testing anything. Over `git://` the clone is the depth-1
//! clone peppy makes of a real remote.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};

/// The line `git daemon --verbose` prints once it accepts connections.
const READY_LINE: &str = "Ready to rumble";

/// How many ports [`GitDaemon::serve`] tries before it gives up. Another
/// process can take the free port between the probe and the daemon's bind.
const PORT_ATTEMPTS: usize = 5;

/// A running `git daemon`, stopped when dropped.
pub(crate) struct GitDaemon {
    child: Child,
    port: u16,
}

impl GitDaemon {
    /// Serves every repository under `base_path`, each at
    /// `git://127.0.0.1:<port>/<directory name>`.
    ///
    /// Returns once the daemon accepts connections: it reads the daemon's
    /// log until the daemon says so, so nothing here waits for a fixed time.
    pub(crate) fn serve(base_path: &Path) -> Self {
        let mut failures = Vec::new();
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port();
            let mut child = Command::new("git")
                .args(["daemon", "--verbose", "--reuseaddr", "--export-all"])
                .arg("--listen=127.0.0.1")
                .arg(format!("--port={port}"))
                .arg(format!("--base-path={}", base_path.display()))
                .arg(base_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("`git daemon` runs (it ships with git)");
            let stderr = child.stderr.take().expect("stderr is piped");
            match wait_until_ready(stderr) {
                Ok(()) => return Self { child, port },
                Err(log) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    failures.push(format!("port {port}: {log}"));
                }
            }
        }
        panic!("`git daemon` never became ready:\n{}", failures.join("\n"));
    }

    /// The URL of the repository in the directory `name` under the base path.
    pub(crate) fn url(&self, name: &str) -> String {
        format!("git://127.0.0.1:{}/{name}", self.port)
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port nothing listens on at the moment of the call.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("bind an ephemeral port")
        .port()
}

/// Reads the daemon's log until it reports that it accepts connections.
///
/// On success the rest of the log is drained on a thread of its own, so a
/// verbose daemon never blocks on a full pipe. On failure (the daemon
/// exited, for example because the port was taken) returns what it logged.
fn wait_until_ready(stderr: ChildStderr) -> Result<(), String> {
    let mut reader = BufReader::new(stderr);
    let mut log = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Err(log),
            Ok(_) if line.contains(READY_LINE) => break,
            Ok(_) => log.push_str(&line),
        }
    }
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    Ok(())
}
