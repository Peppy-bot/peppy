//! Spawns a managed zenohd from a thread that lives as long as the router
//! facade owning it.
//!
//! On Linux the child is armed with `PR_SET_PDEATHSIG(SIGKILL)`. The kernel
//! delivers that signal when the thread that forked the child exits, so the
//! forking thread is one this module owns and keeps alive until the
//! [`ZenohdSpawner`] drops. When the process dies by any path (SIGKILL, abort,
//! OOM kill) the thread dies with it and zenohd ends. The facade stops zenohd
//! itself before it drops the spawner, so on a clean shutdown the thread exits
//! with no child left to signal.
//!
//! Other targets spawn inline; nothing ends zenohd when the process dies.
//!
//! This module holds the crate's one `unsafe` block, the `pre_exec` call (see
//! `lib.rs`).

use std::io;
use std::process::{Child, Command};

#[cfg(target_os = "linux")]
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};

/// Linux keeps 15 bytes of a thread name.
#[cfg(target_os = "linux")]
const THREAD_NAME: &str = "zenohd-spawner";

/// One spawn for the thread to perform, with the channel its result goes to.
#[cfg(target_os = "linux")]
struct Request {
    command: Command,
    reply: SyncSender<io::Result<Child>>,
}

pub(super) struct ZenohdSpawner {
    /// Hands each request to the thread; dropping the last clone ends the thread.
    #[cfg(target_os = "linux")]
    requests: Sender<Request>,
}

impl ZenohdSpawner {
    /// Starts the thread that forks this router's zenohd.
    #[cfg(target_os = "linux")]
    pub(super) fn start() -> io::Result<Self> {
        let (requests, inbox) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name(THREAD_NAME.to_owned())
            .spawn(move || answer_requests(inbox))?;
        Ok(Self { requests })
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn start() -> io::Result<Self> {
        Ok(Self {})
    }

    /// Spawns `command` from the spawner thread and returns its child.
    #[cfg(target_os = "linux")]
    pub(super) fn spawn(&self, command: Command) -> io::Result<Child> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.requests
            .send(Request { command, reply })
            .map_err(|_| spawner_gone())?;
        answer.recv().map_err(|_| spawner_gone())?
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn spawn(&self, mut command: Command) -> io::Result<Child> {
        command.spawn()
    }
}

#[cfg(target_os = "linux")]
fn spawner_gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the zenohd spawner thread has ended",
    )
}

/// The spawner thread: answers each request until every [`ZenohdSpawner`]
/// handle is gone.
#[cfg(target_os = "linux")]
fn answer_requests(inbox: Receiver<Request>) {
    for Request { command, reply } in inbox {
        let spawned = arm(command).spawn();
        // A requester that dropped its `spawn` call cannot own this child.
        if let Err(mpsc::SendError(Ok(mut child))) = reply.send(spawned) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Arms `command` so its child receives SIGKILL when the calling thread exits.
#[cfg(target_os = "linux")]
fn arm(mut command: Command) -> Command {
    use std::os::unix::process::CommandExt;

    let parent = std::process::id();
    // SAFETY: the closure runs in the forked child before exec. `prctl` and
    // `getppid` are thin syscall wrappers that take no locks; the closure
    // captures one `u32`, allocates nothing (`from_raw_os_error` and
    // `last_os_error` build the `Os` variant in place) and touches no parent
    // state.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            // A parent that died between fork and `prctl` sends no signal;
            // `getppid` then names its reaper.
            if libc::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    command
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};

    /// How long the kernel gets to deliver a death signal and the test to see it.
    const DEATH_BOUND: Duration = Duration::from_secs(5);
    /// Time for the kernel to finish signalling every child of an exited thread.
    const SIGNAL_SETTLE: Duration = Duration::from_millis(50);

    fn sleep_command() -> Command {
        let mut command = Command::new("sleep");
        command.arg("30");
        command
    }

    fn exit_within(child: &mut Child, bound: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + bound;
        loop {
            if let Some(status) = child.try_wait().expect("poll the child") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn child_outlives_the_requesting_thread_and_dies_with_the_spawner() {
        let spawner = ZenohdSpawner::start().expect("start the spawner");
        let (mut kept, mut control) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let kept = spawner
                        .spawn(sleep_command())
                        .expect("spawn through the spawner");
                    // Forked on this thread, so it dies when this thread ends.
                    let control = arm(sleep_command()).spawn().expect("spawn here");
                    (kept, control)
                })
                .join()
                .expect("the requesting thread ends")
        });

        // The control's death proves the kernel has processed the thread's exit.
        let control_status =
            exit_within(&mut control, DEATH_BOUND).expect("the control dies with its thread");
        assert_eq!(control_status.signal(), Some(libc::SIGKILL));
        std::thread::sleep(SIGNAL_SETTLE);
        assert!(
            kept.try_wait().expect("poll the kept child").is_none(),
            "the spawner's child keeps running after the requesting thread ended"
        );

        drop(spawner);
        let kept_status =
            exit_within(&mut kept, DEATH_BOUND).expect("the child dies with the spawner");
        assert_eq!(kept_status.signal(), Some(libc::SIGKILL));
    }
}
