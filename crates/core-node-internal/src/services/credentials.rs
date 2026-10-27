//! The keys of the clients of this daemon, and the check of the sender of each
//! call to it.
//!
//! The daemon makes a key pair at each boot, the key of the command line at
//! its first boot, and a key pair at each start of an instance. It writes the
//! key of each client to its key file in the credentials directory, which only
//! the user of the daemon reads; a container node sees its own key file alone
//! (see `node_stack::run_steps`). With a machine certificate, the daemon also
//! writes, beside each key, the machine proof that the daemons of the other
//! machines check: its boot credential and the endorsement of the key.
//!
//! [`DaemonCallCheck`] verifies each call that an endpoint of the daemon
//! receives, before any handler reads the sender: the signature, with the key
//! of a client of this machine or with the key that the machine proof of
//! another machine gives; the timestamp, within [`TIMESTAMP_WINDOW`] of the
//! clock of this daemon; and the nonce, which no call repeats within that
//! window.

use auth::machine_proof::{BootCredential, ClaimedSender, MachineCertificate};
use config::failure_kind::PeppyFailureKind;
use core_node_api::names::COMMAND_LINE_INSTANCE_ID;
use daemon_config::consts::PeppyDirs;
use daemon_config::fs_perms::restrict_dir;
use node_stack::NodeStack;
use parking_lot::Mutex;
use peppylib::messaging::{CallCheck, CallerKind, ReceivedCall, VerifiedCaller};
use pmi::{
    CallProof, CallSigner, ClientCredential, ClientKey, ClientPublicKey, Failure, NONCE_LEN,
    unix_millis,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::services::response::failure;

/// How far the timestamp of a call may be from the clock of the daemon. The
/// machines of a federation keep their wall clocks within it of each other.
pub(crate) const TIMESTAMP_WINDOW: Duration = Duration::from_secs(30);

/// The keys of this daemon and of its clients.
pub(crate) struct DaemonKeys {
    peppy_dirs: PeppyDirs,
    /// The namespace that the daemon runs under, which every boot statement
    /// of its federation names.
    namespace: String,
    core_node: String,
    boot_instance_id: String,
    boot_key: ClientKey,
    /// The certificate of this machine, when it has one: the trust of the
    /// other machines, and the boot credential of this boot.
    machine: Option<(MachineCertificate, BootCredential)>,
    /// The key of the command line, once the daemon wrote its key file.
    command_line: Mutex<Option<ClientPublicKey>>,
    /// The public key of each key file of an instance that is on disk: the
    /// key of a start lives as long as its file, which the process of that
    /// start owns.
    instances: Mutex<HashMap<PathBuf, ClientPublicKey>>,
    /// The serial of the next key file of an instance.
    next_key_serial: AtomicU64,
}

/// The key file of an instance that the daemon wrote for one start of it.
#[derive(Clone, Debug)]
pub(crate) struct IssuedKey {
    pub(crate) path: PathBuf,
}

impl DaemonKeys {
    /// The keys of the boot `boot_instance_id` of the daemon of `core_node`
    /// in `namespace`, with the certificate of its machine when it has one.
    /// A new boot key, in memory alone.
    pub(crate) fn new(
        peppy_dirs: PeppyDirs,
        namespace: &str,
        core_node: &str,
        boot_instance_id: &str,
        machine: Option<MachineCertificate>,
    ) -> Self {
        let boot_key = ClientKey::generate();
        let machine = machine.map(|certificate| {
            let credential = certificate.boot_credential(
                namespace,
                core_node,
                boot_instance_id,
                &boot_key.public_key(),
            );
            (certificate, credential)
        });
        Self {
            peppy_dirs,
            namespace: namespace.to_owned(),
            core_node: core_node.to_owned(),
            boot_instance_id: boot_instance_id.to_owned(),
            boot_key,
            machine,
            command_line: Mutex::new(None),
            instances: Mutex::new(HashMap::new()),
            next_key_serial: AtomicU64::new(0),
        }
    }

    /// The signer of the calls of the daemon itself: the key of its boot,
    /// with its boot credential for the daemons of the other machines.
    pub(crate) fn daemon_signer(&self) -> CallSigner {
        let proof = self
            .machine
            .as_ref()
            .map(|(_, credential)| credential.daemon_proof());
        ClientCredential::new(self.boot_key.clone(), proof).into_signer()
    }

    /// Keys whose directory nothing writes, for the tests that build a run
    /// context and start no instance.
    #[cfg(test)]
    pub(crate) fn unwritten() -> Arc<Self> {
        Arc::new(Self::new(
            PeppyDirs::new("/nonexistent/peppy-root"),
            "local",
            "cn-test",
            "test-boot-0000",
            None,
        ))
    }

    /// Prepares the credentials directory for this boot: removes the key
    /// files of the instances of an earlier boot, and writes the key file of
    /// the command line, with the key that it holds already or with a new
    /// one at the first boot, endorsed by this boot.
    pub(crate) fn prepare(&self) -> std::io::Result<()> {
        let directory = self.peppy_dirs.credentials_dir();
        std::fs::create_dir_all(&directory)?;
        restrict_dir(&directory)?;
        let instances = self.peppy_dirs.instance_keys_dir();
        if instances.exists() {
            std::fs::remove_dir_all(&instances)?;
        }
        std::fs::create_dir_all(&instances)?;
        restrict_dir(&instances)?;

        let path = self.peppy_dirs.command_line_key_path();
        let key = match ClientCredential::read(&path) {
            Ok(credential) => credential.key().clone(),
            Err(error) => {
                if path.exists() {
                    tracing::warn!(
                        %error,
                        "the key file of the command line does not read; the daemon makes a new key"
                    );
                }
                ClientKey::generate()
            }
        };
        let public_key = key.public_key();
        self.credential_of(COMMAND_LINE_INSTANCE_ID, key)
            .write(&path)?;
        *self.command_line.lock() = Some(public_key);
        Ok(())
    }

    /// Writes a new key file for a start of the instance `instance_id`, and
    /// records its key. The daemon takes the key once the process of that
    /// start runs (see [`NodeStack::live_credential`]). The keys whose files
    /// went with their process go from the record.
    pub(crate) fn issue_instance_key(&self, instance_id: &str) -> std::io::Result<IssuedKey> {
        let key = ClientKey::generate();
        let public_key = key.public_key();
        let serial = self.next_key_serial.fetch_add(1, Ordering::Relaxed);
        let path = self.peppy_dirs.instance_key_path(instance_id, serial);
        self.credential_of(instance_id, key).write(&path)?;
        let mut instances = self.instances.lock();
        instances.retain(|file, _| file.exists());
        instances.insert(path.clone(), public_key);
        Ok(IssuedKey { path })
    }

    /// The credential of the client `client` with `key`: with its machine
    /// proof, when this machine has a certificate.
    fn credential_of(&self, client: &str, key: ClientKey) -> ClientCredential {
        let proof = self.machine.as_ref().map(|(_, credential)| {
            credential.client_proof(client, &key.public_key(), &self.boot_key)
        });
        ClientCredential::new(key, proof)
    }

    /// The key and the kind of the client `instance_id` of this machine,
    /// when this daemon gave it a key: the daemon itself under the instance
    /// id of its boot, the command line, or an instance whose process runs,
    /// with the key of the spawn of that process.
    fn local_key(
        &self,
        instance_id: &str,
        node_stack: &NodeStack,
    ) -> Option<(ClientPublicKey, CallerKind)> {
        if instance_id == self.boot_instance_id {
            return Some((self.boot_key.public_key(), CallerKind::Daemon));
        }
        if instance_id == COMMAND_LINE_INSTANCE_ID {
            return self
                .command_line
                .lock()
                .map(|key| (key, CallerKind::CommandLine));
        }
        let key_file = node_stack.live_credential(instance_id)?;
        self.instances
            .lock()
            .get(&key_file)
            .map(|key| (*key, CallerKind::Instance))
    }
}

/// Verifies the sender of each call that an endpoint of the daemon receives.
pub(crate) struct DaemonCallCheck {
    keys: Arc<DaemonKeys>,
    node_stack: Arc<NodeStack>,
    nonces: Mutex<NonceWindow>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl DaemonCallCheck {
    pub(crate) fn new(keys: Arc<DaemonKeys>, node_stack: Arc<NodeStack>) -> Self {
        Self::with_clock(keys, node_stack, Arc::new(SystemTime::now))
    }

    /// The check with `clock` as the clock of the daemon.
    pub(crate) fn with_clock(
        keys: Arc<DaemonKeys>,
        node_stack: Arc<NodeStack>,
        clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    ) -> Self {
        Self {
            keys,
            node_stack,
            nonces: Mutex::new(NonceWindow::default()),
            clock,
        }
    }

    /// The key and the kind of the sender of `call`.
    fn key_of(
        &self,
        call: &ReceivedCall<'_>,
        proof: &CallProof,
        now: SystemTime,
    ) -> Result<(ClientPublicKey, CallerKind), Failure> {
        let sender = Sender::of(call);
        if call.caller_core_node == self.keys.core_node {
            return self
                .keys
                .local_key(call.caller_instance_id, &self.node_stack)
                .ok_or_else(|| {
                    refused(format!(
                        "{sender} has no key on this daemon: the daemon gives a key to the \
                         command line and to each instance that it starts"
                    ))
                });
        }
        let Some((certificate, _)) = &self.keys.machine else {
            return Err(refused(format!(
                "this daemon has no machine certificate, so it takes no call of another machine, \
                 such as the call of {sender}"
            )));
        };
        let machine_proof = proof.machine_proof().ok_or_else(|| {
            refused(format!(
                "the call of {sender}, of another machine, carries no machine proof: the daemon \
                 of that machine has no machine certificate"
            ))
        })?;
        let proven = certificate
            .trust()
            .verify(
                machine_proof,
                ClaimedSender {
                    namespace: &self.keys.namespace,
                    core_node: call.caller_core_node,
                    instance_id: call.caller_instance_id,
                },
                now,
            )
            .map_err(|error| {
                refused(format!(
                    "the machine proof of {sender} does not verify: {error}"
                ))
            })?;
        let kind = if proven.is_daemon {
            CallerKind::Daemon
        } else if call.caller_instance_id == COMMAND_LINE_INSTANCE_ID {
            CallerKind::CommandLine
        } else {
            CallerKind::Instance
        };
        Ok((proven.key, kind))
    }
}

impl CallCheck for DaemonCallCheck {
    fn verify(&self, call: &ReceivedCall<'_>) -> Result<VerifiedCaller, Failure> {
        let sender = Sender::of(call);
        let proof = call.proof.ok_or_else(|| {
            refused(format!(
                "the call of {sender} carries no signature, and the daemon takes signed calls \
                 alone: a node that an earlier peppy built does not sign its calls, so build it \
                 again with this peppy"
            ))
        })?;
        let now = (self.clock)();
        let now_ms = unix_millis(now);
        let skew_ms = now_ms.abs_diff(proof.timestamp_ms());
        let window_ms = TIMESTAMP_WINDOW.as_millis() as u64;
        if skew_ms > window_ms {
            return Err(refused(format!(
                "the timestamp of the call of {sender} is {} s away from the clock of this \
                 daemon, outside the window of {} s: the machines of a federation keep their \
                 clocks within it of each other",
                skew_ms / 1000,
                TIMESTAMP_WINDOW.as_secs()
            )));
        }
        let (key, kind) = self.key_of(call, proof, now)?;
        if !proof.verifies(&key, &call.signed) {
            return Err(refused(format!(
                "the signature of the call does not verify with the key of {sender}"
            )));
        }
        if !self
            .nonces
            .lock()
            .take(key, *proof.nonce(), proof.timestamp_ms(), now_ms, window_ms)
        {
            return Err(refused(format!(
                "the call of {sender} repeats a nonce that this daemon took within {} s: a \
                 replayed call",
                TIMESTAMP_WINDOW.as_secs()
            )));
        }
        Ok(VerifiedCaller::new(
            call.caller_core_node,
            call.caller_instance_id,
            kind,
        ))
    }
}

/// The sender that a call gives, as the messages of a refusal name it.
struct Sender<'a> {
    core_node: &'a str,
    instance_id: &'a str,
}

impl<'a> Sender<'a> {
    fn of(call: &ReceivedCall<'a>) -> Self {
        Self {
            core_node: call.caller_core_node,
            instance_id: call.caller_instance_id,
        }
    }
}

impl std::fmt::Display for Sender<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` of `{}`", self.instance_id, self.core_node)
    }
}

fn refused(message: String) -> Failure {
    failure(PeppyFailureKind::Rejected, message)
}

/// The nonces of the calls that the daemon took within the window, each with
/// the timestamp of its call.
#[derive(Default)]
struct NonceWindow {
    taken: HashMap<(ClientPublicKey, [u8; NONCE_LEN]), u64>,
    /// When the window last dropped the nonces whose calls left it.
    last_prune_ms: u64,
}

impl NonceWindow {
    /// Takes the nonce of a call of `key` signed at `timestamp_ms`: `false`
    /// when a call that the window holds had it already. Drops the nonces of
    /// the calls older than the window, at most once per half window.
    fn take(
        &mut self,
        key: ClientPublicKey,
        nonce: [u8; NONCE_LEN],
        timestamp_ms: u64,
        now_ms: u64,
        window_ms: u64,
    ) -> bool {
        if now_ms.saturating_sub(self.last_prune_ms) > window_ms / 2 {
            let oldest = now_ms.saturating_sub(window_ms);
            self.taken.retain(|_, taken_at| *taken_at >= oldest);
            self.last_prune_ms = now_ms;
        }
        self.taken.insert((key, nonce), timestamp_ms).is_none()
    }
}

/// A check for the tests of the endpoints of the daemon: it knows the key of
/// each test client, and verifies each call with it.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    #[derive(Default)]
    pub(crate) struct KnownKeys {
        keys: HashMap<(String, String), (ClientPublicKey, CallerKind)>,
    }

    impl KnownKeys {
        /// These keys, and the key of `instance_id` of `core_node`, a client
        /// of `kind`.
        pub(crate) fn knowing(
            mut self,
            core_node: &str,
            instance_id: &str,
            key: ClientPublicKey,
            kind: CallerKind,
        ) -> Self {
            self.keys
                .insert((core_node.to_owned(), instance_id.to_owned()), (key, kind));
            self
        }
    }

    impl CallCheck for KnownKeys {
        fn verify(&self, call: &ReceivedCall<'_>) -> Result<VerifiedCaller, Failure> {
            let (key, kind) = self
                .keys
                .get(&(
                    call.caller_core_node.to_owned(),
                    call.caller_instance_id.to_owned(),
                ))
                .ok_or_else(|| refused("no key".to_owned()))?;
            let proof = call
                .proof
                .ok_or_else(|| refused("no signature".to_owned()))?;
            if !proof.verifies(key, &call.signed) {
                return Err(refused("the signature does not verify".to_owned()));
            }
            Ok(VerifiedCaller::new(
                call.caller_core_node,
                call.caller_instance_id,
                *kind,
            ))
        }
    }

    /// A new key, and the signer of the calls of a client with it.
    pub(crate) fn client_key() -> (ClientPublicKey, CallSigner) {
        let key = ClientKey::generate();
        let public_key = key.public_key();
        (public_key, ClientCredential::new(key, None).into_signer())
    }
}

#[cfg(test)]
mod tests;
