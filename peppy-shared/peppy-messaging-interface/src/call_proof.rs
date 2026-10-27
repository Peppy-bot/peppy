//! The proof of the sender of a call to a daemon.
//!
//! A client of a daemon (an instance of a node, the command line, another
//! daemon) signs each call that it sends to a daemon with its Ed25519 key, and
//! the daemon verifies the signature before it reads the sender. The signature
//! covers the namespace of the session, the key expression of the query (which
//! holds the sender and the receiver), the kind of the query, the payload, a
//! timestamp and a nonce ([`SignedCall`]). The proof rides on the attachment
//! of the query, after the kind of the query.
//!
//! A call to the daemon of another machine also carries the machine proof of
//! the key of its sender: the bytes that the daemon of the sender wrote beside
//! the key, which prove the key to the daemons of the other machines. This
//! crate carries them and does not read them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::wire::ServiceQueryKind;

/// The length of the nonce of a proof.
pub const NONCE_LEN: usize = 16;
/// The length of an Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;
/// The length of an Ed25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;

/// The first byte of an encoded proof.
const PROOF_VERSION: u8 = 0x01;

/// The context that starts the signed bytes of a call, so a signature of a
/// call is never a valid signature of another kind of statement.
const SIGNED_CALL_CONTEXT: &[u8] = b"peppy/call-proof/v1";

/// The public key of a client of a daemon.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientPublicKey([u8; PUBLIC_KEY_LEN]);

impl ClientPublicKey {
    /// The key whose bytes are `bytes`, when they are one Ed25519 public key.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.0
    }

    /// Whether `signature` is the signature of `message` by this key.
    pub fn verifies(&self, message: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ED25519, &self.0)
            .verify(message, signature)
            .is_ok()
    }
}

impl fmt::Display for ClientPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&BASE64.encode(self.0))
    }
}

impl fmt::Debug for ClientPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClientPublicKey({self})")
    }
}

/// The Ed25519 key pair of a client of a daemon. It keeps its PKCS#8 document,
/// which its file holds.
pub struct ClientKey {
    pair: Ed25519KeyPair,
    pkcs8: Vec<u8>,
}

impl ClientKey {
    /// A new key pair.
    pub fn generate() -> Self {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("the system random source gives the bytes of a key pair");
        Self::from_pkcs8(document.as_ref())
            .expect("a PKCS#8 document that ring generated parses again")
    }

    /// The key pair of the PKCS#8 document `pkcs8`.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, CredentialError> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|error| CredentialError::Key(error.to_string()))?;
        Ok(Self {
            pair,
            pkcs8: pkcs8.to_vec(),
        })
    }

    pub fn public_key(&self) -> ClientPublicKey {
        ClientPublicKey::from_bytes(self.pair.public_key().as_ref())
            .expect("an Ed25519 public key has 32 bytes")
    }

    /// The signature of `message` by this key.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        self.pair
            .sign(message)
            .as_ref()
            .try_into()
            .expect("an Ed25519 signature has 64 bytes")
    }
}

impl Clone for ClientKey {
    fn clone(&self) -> Self {
        Self::from_pkcs8(&self.pkcs8).expect("the PKCS#8 document of a key parses again")
    }
}

impl fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// Why a key file of a client was not read.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("the key file `{path}` could not be read: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("the key file `{path}` is not a key file of peppy: {reason}")]
    Format { path: String, reason: String },
    #[error("the key is not an Ed25519 key pair in PKCS#8: {0}")]
    Key(String),
}

/// The file form of a [`ClientCredential`]: base64 of the PKCS#8 document of
/// the key and of the machine proof.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine_proof: Option<String>,
}

/// The key that a client signs its calls to a daemon with, and the machine
/// proof of that key when the daemon that gave the key has one. A daemon
/// writes it to the key file of the client, which only the user of the daemon
/// can read.
#[derive(Debug)]
pub struct ClientCredential {
    key: ClientKey,
    machine_proof: Option<Bytes>,
}

impl ClientCredential {
    pub fn new(key: ClientKey, machine_proof: Option<Bytes>) -> Self {
        Self { key, machine_proof }
    }

    pub fn key(&self) -> &ClientKey {
        &self.key
    }

    pub fn machine_proof(&self) -> Option<&Bytes> {
        self.machine_proof.as_ref()
    }

    /// The credential in the key file at `path`.
    pub fn read(path: &Path) -> Result<Self, CredentialError> {
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| CredentialError::Read {
            path: shown.clone(),
            source,
        })?;
        let format = |reason: String| CredentialError::Format {
            path: shown.clone(),
            reason,
        };
        let file: CredentialFile =
            serde_json::from_str(&text).map_err(|error| format(error.to_string()))?;
        let pkcs8 = BASE64
            .decode(&file.key)
            .map_err(|error| format(format!("the key is not base64: {error}")))?;
        let machine_proof = file
            .machine_proof
            .map(|proof| BASE64.decode(proof).map(Bytes::from))
            .transpose()
            .map_err(|error| format(format!("the machine proof is not base64: {error}")))?;
        Ok(Self {
            key: ClientKey::from_pkcs8(&pkcs8)?,
            machine_proof,
        })
    }

    /// Writes the credential to the key file at `path`, whole: a reader sees
    /// the earlier file or this one, never a part. Only the user who writes
    /// it can read the file (mode 0600).
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let file = CredentialFile {
            key: BASE64.encode(&self.key.pkcs8),
            machine_proof: self
                .machine_proof
                .as_ref()
                .map(|proof| BASE64.encode(proof)),
        };
        let text = serde_json::to_string(&file).map_err(std::io::Error::other)?;
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        // A temporary file has mode 0600 from its creation, so the key is
        // never readable by another user, even for a moment.
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    /// The signer of the calls of this client.
    pub fn into_signer(self) -> CallSigner {
        CallSigner {
            key: self.key,
            machine_proof: self.machine_proof,
        }
    }
}

/// Who receives a signed call: the daemon of the machine of the sender, or
/// the daemon of another machine, which reads the machine proof of the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallReceiver {
    ThisMachine,
    OtherMachine,
}

/// Signs the calls of one client.
#[derive(Debug)]
pub struct CallSigner {
    key: ClientKey,
    machine_proof: Option<Bytes>,
}

impl CallSigner {
    pub fn public_key(&self) -> ClientPublicKey {
        self.key.public_key()
    }

    /// The proof of `call` for `receiver`, signed now with a new nonce.
    pub fn prove(&self, call: &SignedCall<'_>, receiver: CallReceiver) -> CallProof {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("the system random source gives the bytes of a nonce");
        let timestamp_ms = unix_millis(SystemTime::now());
        let signature = self.key.sign(&call.signed_bytes(timestamp_ms, &nonce));
        let machine_proof = match receiver {
            CallReceiver::ThisMachine => None,
            CallReceiver::OtherMachine => self.machine_proof.clone(),
        };
        CallProof {
            timestamp_ms,
            nonce,
            signature,
            machine_proof,
        }
    }
}

/// The milliseconds of `time` since the Unix epoch; 0 before it.
pub fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

/// How a sender signs one call: with which signer, for which receiver. It
/// rides on the [`crate::ServiceWireSender`] of the call. Two are equal when
/// they hold the same signer for the same receiver.
#[derive(Clone)]
pub struct CallSigning {
    signer: Arc<CallSigner>,
    receiver: CallReceiver,
}

impl CallSigning {
    pub fn new(signer: Arc<CallSigner>, receiver: CallReceiver) -> Self {
        Self { signer, receiver }
    }

    /// The proof of `call`.
    pub(crate) fn prove(&self, call: &SignedCall<'_>) -> CallProof {
        self.signer.prove(call, self.receiver)
    }
}

impl PartialEq for CallSigning {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.signer, &other.signer) && self.receiver == other.receiver
    }
}

impl Eq for CallSigning {}

impl fmt::Debug for CallSigning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallSigning")
            .field("public_key", &self.signer.public_key())
            .field("receiver", &self.receiver)
            .finish()
    }
}

/// What the proof of a call signs, apart from its timestamp and its nonce.
#[derive(Clone, Copy, Debug)]
pub struct SignedCall<'a> {
    /// The namespace of the session that sends or receives the call.
    pub namespace: &'a str,
    /// The key expression of the query, without the namespace.
    pub key_expr: &'a str,
    pub kind: ServiceQueryKind,
    pub payload: &'a [u8],
}

impl SignedCall<'_> {
    /// The bytes that the signature of the call covers. Each field of a
    /// variable length carries its length, so two different calls never give
    /// the same bytes.
    pub fn signed_bytes(&self, timestamp_ms: u64, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            SIGNED_CALL_CONTEXT.len()
                + 3 * 8
                + self.namespace.len()
                + self.key_expr.len()
                + 1
                + self.payload.len()
                + 8
                + NONCE_LEN,
        );
        bytes.extend_from_slice(SIGNED_CALL_CONTEXT);
        for field in [self.namespace.as_bytes(), self.key_expr.as_bytes()] {
            bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        bytes.push(self.kind.as_byte());
        bytes.extend_from_slice(&(self.payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(self.payload);
        bytes.extend_from_slice(&timestamp_ms.to_be_bytes());
        bytes.extend_from_slice(nonce);
        bytes
    }
}

/// The proof that a signed call carries.
#[derive(Clone, PartialEq, Eq)]
pub struct CallProof {
    timestamp_ms: u64,
    nonce: [u8; NONCE_LEN],
    signature: [u8; SIGNATURE_LEN],
    machine_proof: Option<Bytes>,
}

impl CallProof {
    /// When the sender signed the call, in milliseconds since the Unix epoch.
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    pub fn nonce(&self) -> &[u8; NONCE_LEN] {
        &self.nonce
    }

    /// The machine proof of the key of the sender, which a call to the daemon
    /// of another machine carries.
    pub fn machine_proof(&self) -> Option<&Bytes> {
        self.machine_proof.as_ref()
    }

    /// Whether the signature of this proof is the signature of `call` by
    /// `key`.
    pub fn verifies(&self, key: &ClientPublicKey, call: &SignedCall<'_>) -> bool {
        key.verifies(
            &call.signed_bytes(self.timestamp_ms, &self.nonce),
            &self.signature,
        )
    }

    /// Appends the encoded proof to `out`: the version, the timestamp, the
    /// nonce, the signature, then the length and the bytes of the machine
    /// proof (length 0 when the call carries none).
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(PROOF_VERSION);
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.signature);
        let machine_proof = self.machine_proof.as_deref().unwrap_or_default();
        out.extend_from_slice(&(machine_proof.len() as u32).to_be_bytes());
        out.extend_from_slice(machine_proof);
    }

    /// The proof encoded in `bytes`, which hold it whole and nothing after it.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, CallProofDecodeError> {
        let mut reader = Reader(bytes);
        let version = reader.take::<1>()?[0];
        if version != PROOF_VERSION {
            return Err(CallProofDecodeError::UnknownVersion(version));
        }
        let timestamp_ms = u64::from_be_bytes(reader.take::<8>()?);
        let nonce = reader.take::<NONCE_LEN>()?;
        let signature = reader.take::<SIGNATURE_LEN>()?;
        let machine_proof_len = u32::from_be_bytes(reader.take::<4>()?) as usize;
        let machine_proof = reader.take_slice(machine_proof_len)?;
        if !reader.0.is_empty() {
            return Err(CallProofDecodeError::TrailingBytes(reader.0.len()));
        }
        Ok(Self {
            timestamp_ms,
            nonce,
            signature,
            machine_proof: (!machine_proof.is_empty())
                .then(|| Bytes::copy_from_slice(machine_proof)),
        })
    }
}

impl fmt::Debug for CallProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallProof")
            .field("timestamp_ms", &self.timestamp_ms)
            .field(
                "machine_proof_len",
                &self.machine_proof.as_ref().map(Bytes::len),
            )
            .finish_non_exhaustive()
    }
}

/// Why the proof on the attachment of a query does not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallProofDecodeError {
    #[error("the proof of the call is cut short")]
    Truncated,
    #[error("the proof of the call has the unknown version {0}")]
    UnknownVersion(u8),
    #[error("the proof of the call has {0} bytes after its end")]
    TrailingBytes(usize),
}

/// Reads the fields of an encoded proof in order.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], CallProofDecodeError> {
        let field = self.take_slice(N)?;
        Ok(field.try_into().expect("the slice has N bytes"))
    }

    fn take_slice(&mut self, len: usize) -> Result<&'a [u8], CallProofDecodeError> {
        if self.0.len() < len {
            return Err(CallProofDecodeError::Truncated);
        }
        let (field, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(payload: &[u8]) -> SignedCall<'_> {
        SignedCall {
            namespace: "local",
            key_expr: "cn-desk/cn-desk/*/peppy-cli/service/node/cn-desk/core/*/stack_reset",
            kind: ServiceQueryKind::UserRequest,
            payload,
        }
    }

    fn signer(machine_proof: Option<&'static [u8]>) -> CallSigner {
        ClientCredential::new(ClientKey::generate(), machine_proof.map(Bytes::from_static))
            .into_signer()
    }

    #[test]
    fn a_proof_verifies_its_call_with_the_key_of_its_signer_alone() {
        let signer = signer(None);
        let proof = signer.prove(&call(b"reset"), CallReceiver::ThisMachine);
        assert!(proof.verifies(&signer.public_key(), &call(b"reset")));

        let other = ClientKey::generate().public_key();
        assert!(!proof.verifies(&other, &call(b"reset")), "another key");
    }

    #[test]
    fn a_proof_does_not_verify_a_call_that_differs_in_any_signed_field() {
        let signer = signer(None);
        let key = signer.public_key();
        let proof = signer.prove(&call(b"reset"), CallReceiver::ThisMachine);

        assert!(!proof.verifies(&key, &call(b"other")), "the payload");
        let mut changed = call(b"reset");
        changed.namespace = "ws-other";
        assert!(!proof.verifies(&key, &changed), "the namespace");
        let mut changed = call(b"reset");
        changed.key_expr = "cn-desk/cn-desk/*/arm_inst/service/node/cn-desk/core/*/stack_reset";
        assert!(
            !proof.verifies(&key, &changed),
            "the sender in the key expression"
        );
        let mut changed = call(b"reset");
        changed.kind = ServiceQueryKind::Probe;
        assert!(!proof.verifies(&key, &changed), "the kind");

        let mut later = proof.clone();
        later.timestamp_ms += 1;
        assert!(!later.verifies(&key, &call(b"reset")), "the timestamp");
        let mut renonced = proof;
        renonced.nonce[0] ^= 1;
        assert!(!renonced.verifies(&key, &call(b"reset")), "the nonce");
    }

    #[test]
    fn the_namespace_and_the_key_expression_cannot_trade_bytes() {
        let signer = signer(None);
        let first = SignedCall {
            namespace: "ab",
            key_expr: "c",
            ..call(b"")
        };
        let second = SignedCall {
            namespace: "a",
            key_expr: "bc",
            ..call(b"")
        };
        let proof = signer.prove(&first, CallReceiver::ThisMachine);
        assert!(!proof.verifies(&signer.public_key(), &second));
    }

    #[test]
    fn the_machine_proof_rides_on_a_call_to_another_machine_alone() {
        let signer = signer(Some(b"endorsement and boot credential"));
        let local = signer.prove(&call(b"x"), CallReceiver::ThisMachine);
        let remote = signer.prove(&call(b"x"), CallReceiver::OtherMachine);
        assert_eq!(local.machine_proof(), None);
        assert_eq!(
            remote.machine_proof().map(|proof| proof.as_ref()),
            Some(&b"endorsement and boot credential"[..])
        );
    }

    #[test]
    fn a_proof_decodes_to_itself_and_a_damaged_one_is_refused() {
        let signer = signer(Some(b"machine"));
        for receiver in [CallReceiver::ThisMachine, CallReceiver::OtherMachine] {
            let proof = signer.prove(&call(b"x"), receiver);
            let mut encoded = Vec::new();
            proof.encode_into(&mut encoded);
            assert_eq!(CallProof::decode(&encoded), Ok(proof));

            assert_eq!(
                CallProof::decode(&encoded[..encoded.len() - 1]),
                Err(CallProofDecodeError::Truncated)
            );
            let mut longer = encoded.clone();
            longer.push(0);
            assert_eq!(
                CallProof::decode(&longer),
                Err(CallProofDecodeError::TrailingBytes(1))
            );
            let mut versioned = encoded;
            versioned[0] = 9;
            assert_eq!(
                CallProof::decode(&versioned),
                Err(CallProofDecodeError::UnknownVersion(9))
            );
        }
    }

    #[test]
    fn two_proofs_of_one_call_have_different_nonces() {
        let signer = signer(None);
        let first = signer.prove(&call(b"x"), CallReceiver::ThisMachine);
        let second = signer.prove(&call(b"x"), CallReceiver::ThisMachine);
        assert_ne!(first.nonce(), second.nonce());
    }

    #[test]
    fn a_key_file_reads_back_as_the_credential_it_holds_and_only_its_user_reads_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("arm_inst.key");
        let credential =
            ClientCredential::new(ClientKey::generate(), Some(Bytes::from_static(b"proof")));
        credential.write(&path).unwrap();

        let read = ClientCredential::read(&path).unwrap();
        assert_eq!(read.key().public_key(), credential.key().public_key());
        assert_eq!(read.machine_proof(), credential.machine_proof());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let bare = ClientCredential::new(ClientKey::generate(), None);
        bare.write(&path).unwrap();
        assert_eq!(ClientCredential::read(&path).unwrap().machine_proof(), None);
    }

    #[test]
    fn a_file_that_is_not_a_key_file_is_refused_with_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.key");
        std::fs::write(&path, r#"{"key":"not base64!"}"#).unwrap();
        let error = ClientCredential::read(&path).unwrap_err().to_string();
        assert!(error.contains("broken.key"), "{error}");
        assert!(error.contains("not base64"), "{error}");

        let missing = ClientCredential::read(&directory.path().join("missing.key"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("missing.key"), "{missing}");
    }
}
