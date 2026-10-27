//! The proof of a key to the daemons of the other machines of a federation.
//!
//! A machine proves its identity with a certificate of a CA and the private
//! key of that certificate, its [`MachineCertificate`]: the platform
//! certificate of its enrollment, or a certificate that the operator of an
//! operator-run router gives. At each boot its daemon signs a boot statement
//! with that key: the namespace, the core node, the instance id of the boot
//! and the public key of the boot. With the certificate chain, the statement
//! is the [`BootCredential`] of the daemon. The daemon then endorses, with the
//! key of its boot, the key of each client that it gives a key to: the
//! command line and each instance that it starts.
//!
//! A call to the daemon of another machine carries the machine proof of the
//! key of its sender: the boot credential, and the endorsement of the key
//! when the sender is not the daemon itself. The receiver checks the chain
//! against its own CA ([`MachineTrust`]), the statement, and the endorsement,
//! and that they name the namespace, the core node and the instance id that
//! the call gives. It then verifies the signature of the call with the key
//! that the proof gives.
//!
//! A certificate of an operator CA names the core node of its machine in its
//! common name. A platform certificate names a peer of the project, not a
//! core node, so a machine of the project can give the core node name of
//! another machine of the project.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use pmi::{ClientKey, ClientPublicKey};
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P384_SHA384_ASN1_SIGNING, EcdsaKeyPair, Ed25519KeyPair,
    RSA_PKCS1_SHA256, RsaKeyPair,
};
use rustls_pki_types::{CertificateDer, SignatureVerificationAlgorithm, TrustAnchor, UnixTime};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::time::SystemTime;
use webpki::{EndEntityCert, KeyUsage};

use crate::enrollment::Enrollment;

/// The context that starts the signed bytes of a boot statement.
const BOOT_STATEMENT_CONTEXT: &[u8] = b"peppy/boot-statement/v1";

/// The context that starts the signed bytes of an endorsement.
const ENDORSEMENT_CONTEXT: &[u8] = b"peppy/endorsement/v1";

/// The algorithms that a certificate chain of a machine may be signed with.
static CHAIN_ALGORITHMS: &[&dyn SignatureVerificationAlgorithm] = &[
    webpki::ring::ECDSA_P256_SHA256,
    webpki::ring::ECDSA_P256_SHA384,
    webpki::ring::ECDSA_P384_SHA256,
    webpki::ring::ECDSA_P384_SHA384,
    webpki::ring::ED25519,
    webpki::ring::RSA_PKCS1_2048_8192_SHA256,
    webpki::ring::RSA_PKCS1_2048_8192_SHA384,
    webpki::ring::RSA_PKCS1_2048_8192_SHA512,
    webpki::ring::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
    webpki::ring::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
    webpki::ring::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
];

/// Why a machine certificate, a boot credential or a machine proof is not
/// usable.
#[derive(Debug, thiserror::Error)]
pub enum MachineProofError {
    #[error("the file `{path}` could not be read: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("`{path}` holds no PEM {label}")]
    NoPem { path: String, label: &'static str },
    #[error("the private key of the machine certificate is not a PKCS#8 key of ECDSA P-256, ECDSA P-384, Ed25519 or RSA")]
    UnsupportedKey,
    #[error("the machine proof does not decode: {0}")]
    Decode(String),
    #[error("the machine proof names {field} `{given}`, but the call gives `{expected}`")]
    NamesAnother {
        field: &'static str,
        given: String,
        expected: String,
    },
    #[error("the certificate of the machine does not chain to the CA of this daemon: {0}")]
    Chain(webpki::Error),
    #[error("the certificate of the machine names `{named}` in its common name, not the core node `{core_node}`")]
    CommonName { named: String, core_node: String },
    #[error("the boot statement does not verify with the key of the certificate of the machine")]
    BootStatement,
    #[error("the machine proof endorses no key for `{0}`")]
    NoEndorsement(String),
    #[error("the endorsement does not verify with the key of the boot of the machine")]
    Endorsement,
}

/// How the certificates of a CA name the machine of a daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaKind {
    /// The CA of a project of the platform: a certificate names a peer of the
    /// project, not a core node.
    Platform,
    /// A CA of the operator of an operator-run router: a certificate names
    /// the core node of its machine in its common name.
    Operator,
}

/// The CA that the daemons of a federation check the machines against.
pub struct MachineTrust {
    anchors: Vec<TrustAnchor<'static>>,
    kind: CaKind,
}

impl fmt::Debug for MachineTrust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachineTrust")
            .field("anchors", &self.anchors.len())
            .field("kind", &self.kind)
            .finish()
    }
}

/// What a verified machine proof gives: the key of the sender, and whether
/// the sender is the daemon of the machine, which signs with the key of its
/// boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProvenKey {
    pub key: ClientPublicKey,
    pub is_daemon: bool,
}

/// The sender that a call gives, which a machine proof must name.
#[derive(Clone, Copy, Debug)]
pub struct ClaimedSender<'a> {
    pub namespace: &'a str,
    pub core_node: &'a str,
    pub instance_id: &'a str,
}

impl MachineTrust {
    /// The CA in the PEM file at `path`, whose certificates name machines as
    /// `kind` says.
    pub fn read(path: &Path, kind: CaKind) -> Result<Self, MachineProofError> {
        let anchors = read_certificates(path)?
            .iter()
            .map(|certificate| {
                webpki::anchor_from_trusted_cert(certificate)
                    .map(|anchor| anchor.to_owned())
                    .map_err(MachineProofError::Chain)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { anchors, kind })
    }

    /// The key that `proof` gives to `sender`, checked against this CA at
    /// `now`.
    pub fn verify(
        &self,
        proof: &[u8],
        sender: ClaimedSender<'_>,
        now: SystemTime,
    ) -> Result<ProvenKey, MachineProofError> {
        let proof = MachineProof::decode(proof)?;
        let boot = &proof.boot;
        expect_name("the namespace", &boot.namespace, sender.namespace)?;
        expect_name("the core node", &boot.core_node, sender.core_node)?;
        let leaf = self.verify_chain(&boot.certificates, now)?;
        if self.kind == CaKind::Operator {
            let named = common_name(&boot.certificates[0])?;
            if named != boot.core_node {
                return Err(MachineProofError::CommonName {
                    named,
                    core_node: boot.core_node.clone(),
                });
            }
        }
        leaf.verify_signature(
            boot.algorithm.verification(),
            &boot.signed_bytes(),
            &boot.signature,
        )
        .map_err(|_| MachineProofError::BootStatement)?;
        if sender.instance_id == boot.instance_id {
            return Ok(ProvenKey {
                key: boot.public_key,
                is_daemon: true,
            });
        }
        let endorsement = proof
            .endorsement
            .filter(|endorsement| endorsement.client == sender.instance_id)
            .ok_or_else(|| MachineProofError::NoEndorsement(sender.instance_id.to_owned()))?;
        if !boot
            .public_key
            .verifies(&endorsement.signed_bytes(boot), &endorsement.signature)
        {
            return Err(MachineProofError::Endorsement);
        }
        Ok(ProvenKey {
            key: endorsement.public_key,
            is_daemon: false,
        })
    }

    /// The leaf of `certificates`, once the chain checks against this CA at
    /// `now` for client authentication.
    fn verify_chain<'a>(
        &self,
        certificates: &'a [CertificateDer<'static>],
        now: SystemTime,
    ) -> Result<EndEntityCert<'a>, MachineProofError> {
        let (leaf, intermediates) = certificates
            .split_first()
            .ok_or_else(|| MachineProofError::Decode("the chain holds no certificate".into()))?;
        let leaf = EndEntityCert::try_from(leaf).map_err(MachineProofError::Chain)?;
        let time = UnixTime::since_unix_epoch(
            now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default(),
        );
        leaf.verify_for_usage(
            CHAIN_ALGORITHMS,
            &self.anchors,
            intermediates,
            time,
            KeyUsage::client_auth(),
            None,
            None,
        )
        .map_err(MachineProofError::Chain)?;
        Ok(leaf)
    }
}

fn expect_name(field: &'static str, given: &str, expected: &str) -> Result<(), MachineProofError> {
    if given == expected {
        return Ok(());
    }
    Err(MachineProofError::NamesAnother {
        field,
        given: given.to_owned(),
        expected: expected.to_owned(),
    })
}

/// The common name of the subject of the certificate `der`.
fn common_name(der: &CertificateDer<'_>) -> Result<String, MachineProofError> {
    let (_, certificate) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|error| MachineProofError::Decode(error.to_string()))?;
    let name = certificate
        .subject()
        .iter_common_name()
        .next()
        .and_then(|name| name.as_str().ok())
        .unwrap_or_default();
    Ok(name.to_owned())
}

/// The certificate of this machine, its private key, and the CA that the
/// daemons of the federation check machines against.
pub struct MachineCertificate {
    /// The leaf, then the certificates above it.
    chain: Vec<CertificateDer<'static>>,
    key: MachineKey,
    trust: MachineTrust,
}

impl fmt::Debug for MachineCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachineCertificate")
            .field("chain", &self.chain.len())
            .field("trust", &self.trust)
            .finish_non_exhaustive()
    }
}

impl MachineCertificate {
    /// The platform certificate of `enrollment`: its leaf and its issuer
    /// chain, its key, and the CA of its project.
    pub fn of_enrollment(enrollment: &Enrollment) -> Result<Self, MachineProofError> {
        let mut chain = read_certificates(&enrollment.peer_certificate)?;
        chain.extend(read_certificates(&enrollment.issuer_chain)?);
        Ok(Self {
            chain,
            key: MachineKey::read(&enrollment.peer_key)?,
            trust: MachineTrust::read(&enrollment.trust_anchor, CaKind::Platform)?,
        })
    }

    /// The certificate that the operator gives: `certificate` holds the leaf
    /// then the certificates above it, `private_key` its key, and
    /// `trust_anchor` the CA of the operator.
    pub fn of_operator(
        certificate: &Path,
        private_key: &Path,
        trust_anchor: &Path,
    ) -> Result<Self, MachineProofError> {
        Ok(Self {
            chain: read_certificates(certificate)?,
            key: MachineKey::read(private_key)?,
            trust: MachineTrust::read(trust_anchor, CaKind::Operator)?,
        })
    }

    /// The CA that the daemon checks the other machines against.
    pub fn trust(&self) -> &MachineTrust {
        &self.trust
    }

    /// The boot credential of the boot `instance_id` of the daemon of
    /// `core_node`, in `namespace`, whose key is `boot_key`.
    pub fn boot_credential(
        &self,
        namespace: &str,
        core_node: &str,
        instance_id: &str,
        boot_key: &ClientPublicKey,
    ) -> BootCredential {
        let mut statement = BootStatement {
            namespace: namespace.to_owned(),
            core_node: core_node.to_owned(),
            instance_id: instance_id.to_owned(),
            public_key: *boot_key,
            algorithm: self.key.algorithm(),
            signature: Vec::new(),
            certificates: self.chain.clone(),
        };
        statement.signature = self.key.sign(&statement.signed_bytes());
        BootCredential { statement }
    }
}

/// The boot credential of a daemon: its boot statement, signed with the key
/// of its machine certificate, and the certificate chain.
#[derive(Clone)]
pub struct BootCredential {
    statement: BootStatement,
}

impl fmt::Debug for BootCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BootCredential")
            .field("core_node", &self.statement.core_node)
            .field("instance_id", &self.statement.instance_id)
            .finish_non_exhaustive()
    }
}

impl BootCredential {
    /// The machine proof of the key of the boot itself, which the daemon
    /// carries on its calls to the daemons of the other machines.
    pub fn daemon_proof(&self) -> Bytes {
        MachineProof {
            boot: self.statement.clone(),
            endorsement: None,
        }
        .encode()
    }

    /// The machine proof of `client_key`, the key of the client `client` of
    /// the daemon, endorsed by `boot_key`, the key of the boot.
    pub fn client_proof(
        &self,
        client: &str,
        client_key: &ClientPublicKey,
        boot_key: &ClientKey,
    ) -> Bytes {
        let mut endorsement = Endorsement {
            client: client.to_owned(),
            public_key: *client_key,
            signature: Vec::new(),
        };
        endorsement.signature = boot_key
            .sign(&endorsement.signed_bytes(&self.statement))
            .to_vec();
        MachineProof {
            boot: self.statement.clone(),
            endorsement: Some(endorsement),
        }
        .encode()
    }
}

/// The signature algorithm of a boot statement: the one of the key of the
/// machine certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MachineSignature {
    EcdsaP256Sha256,
    EcdsaP384Sha384,
    Ed25519,
    RsaPkcs1Sha256,
}

impl MachineSignature {
    fn verification(self) -> &'static dyn SignatureVerificationAlgorithm {
        match self {
            Self::EcdsaP256Sha256 => webpki::ring::ECDSA_P256_SHA256,
            Self::EcdsaP384Sha384 => webpki::ring::ECDSA_P384_SHA384,
            Self::Ed25519 => webpki::ring::ED25519,
            Self::RsaPkcs1Sha256 => webpki::ring::RSA_PKCS1_2048_8192_SHA256,
        }
    }
}

/// The private key of a machine certificate.
enum MachineKey {
    EcdsaP256(EcdsaKeyPair),
    EcdsaP384(EcdsaKeyPair),
    Ed25519(Ed25519KeyPair),
    Rsa(RsaKeyPair),
}

impl MachineKey {
    /// The PKCS#8 private key in the PEM file at `path`.
    fn read(path: &Path) -> Result<Self, MachineProofError> {
        let der = read_pem(path, "PRIVATE KEY")?
            .into_iter()
            .next()
            .ok_or_else(|| MachineProofError::NoPem {
                path: path.display().to_string(),
                label: "PRIVATE KEY",
            })?;
        Self::from_pkcs8(&der)
    }

    fn from_pkcs8(der: &[u8]) -> Result<Self, MachineProofError> {
        let random = SystemRandom::new();
        if let Ok(pair) = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, der, &random) {
            return Ok(Self::EcdsaP256(pair));
        }
        if let Ok(pair) = EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, der, &random) {
            return Ok(Self::EcdsaP384(pair));
        }
        if let Ok(pair) = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der) {
            return Ok(Self::Ed25519(pair));
        }
        RsaKeyPair::from_pkcs8(der)
            .map(Self::Rsa)
            .map_err(|_| MachineProofError::UnsupportedKey)
    }

    fn algorithm(&self) -> MachineSignature {
        match self {
            Self::EcdsaP256(_) => MachineSignature::EcdsaP256Sha256,
            Self::EcdsaP384(_) => MachineSignature::EcdsaP384Sha384,
            Self::Ed25519(_) => MachineSignature::Ed25519,
            Self::Rsa(_) => MachineSignature::RsaPkcs1Sha256,
        }
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        let random = SystemRandom::new();
        match self {
            Self::EcdsaP256(pair) | Self::EcdsaP384(pair) => pair
                .sign(&random, message)
                .expect("an ECDSA key signs with the system random source")
                .as_ref()
                .to_vec(),
            Self::Ed25519(pair) => pair.sign(message).as_ref().to_vec(),
            Self::Rsa(pair) => {
                let mut signature = vec![0; pair.public().modulus_len()];
                pair.sign(&RSA_PKCS1_SHA256, &random, message, &mut signature)
                    .expect("an RSA key signs into a buffer of its modulus length");
                signature
            }
        }
    }
}

/// What a daemon states at its boot, signed with the key of its machine
/// certificate.
#[derive(Clone)]
struct BootStatement {
    namespace: String,
    core_node: String,
    instance_id: String,
    public_key: ClientPublicKey,
    algorithm: MachineSignature,
    signature: Vec<u8>,
    certificates: Vec<CertificateDer<'static>>,
}

impl BootStatement {
    fn signed_bytes(&self) -> Vec<u8> {
        signed_bytes(
            BOOT_STATEMENT_CONTEXT,
            &[&self.namespace, &self.core_node, &self.instance_id],
            &self.public_key,
        )
    }
}

/// The endorsement of the key of a client of a daemon, signed with the key
/// of the boot of the daemon.
#[derive(Clone)]
struct Endorsement {
    client: String,
    public_key: ClientPublicKey,
    signature: Vec<u8>,
}

impl Endorsement {
    /// The bytes that the key of `boot` signs: the namespace, the core node
    /// and the boot that `boot` states, the client and its key.
    fn signed_bytes(&self, boot: &BootStatement) -> Vec<u8> {
        signed_bytes(
            ENDORSEMENT_CONTEXT,
            &[
                &boot.namespace,
                &boot.core_node,
                &boot.instance_id,
                &self.client,
            ],
            &self.public_key,
        )
    }
}

/// `context`, then each of `fields` after its length, then `key`.
fn signed_bytes(context: &[u8], fields: &[&str], key: &ClientPublicKey) -> Vec<u8> {
    let mut bytes = context.to_vec();
    for field in fields {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    bytes.extend_from_slice(key.as_bytes());
    bytes
}

/// A machine proof: the boot credential of the daemon of the sender, and the
/// endorsement of the key of the sender when the sender is a client of that
/// daemon.
struct MachineProof {
    boot: BootStatement,
    endorsement: Option<Endorsement>,
}

/// The JSON form of a [`MachineProof`], with base64 for the bytes.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineProofJson {
    boot: BootStatementJson,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endorsement: Option<EndorsementJson>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootStatementJson {
    namespace: String,
    core_node: String,
    instance_id: String,
    public_key: String,
    algorithm: MachineSignature,
    signature: String,
    certificates: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndorsementJson {
    client: String,
    public_key: String,
    signature: String,
}

impl MachineProof {
    fn encode(&self) -> Bytes {
        let boot = &self.boot;
        let json = MachineProofJson {
            boot: BootStatementJson {
                namespace: boot.namespace.clone(),
                core_node: boot.core_node.clone(),
                instance_id: boot.instance_id.clone(),
                public_key: BASE64.encode(boot.public_key.as_bytes()),
                algorithm: boot.algorithm,
                signature: BASE64.encode(&boot.signature),
                certificates: boot
                    .certificates
                    .iter()
                    .map(|certificate| BASE64.encode(certificate))
                    .collect(),
            },
            endorsement: self.endorsement.as_ref().map(|endorsement| EndorsementJson {
                client: endorsement.client.clone(),
                public_key: BASE64.encode(endorsement.public_key.as_bytes()),
                signature: BASE64.encode(&endorsement.signature),
            }),
        };
        Bytes::from(serde_json::to_vec(&json).expect("a machine proof serializes"))
    }

    fn decode(bytes: &[u8]) -> Result<Self, MachineProofError> {
        let json: MachineProofJson = serde_json::from_slice(bytes)
            .map_err(|error| MachineProofError::Decode(error.to_string()))?;
        let boot = json.boot;
        Ok(Self {
            boot: BootStatement {
                namespace: boot.namespace,
                core_node: boot.core_node,
                instance_id: boot.instance_id,
                public_key: decode_key(&boot.public_key)?,
                algorithm: boot.algorithm,
                signature: decode_base64(&boot.signature)?,
                certificates: boot
                    .certificates
                    .iter()
                    .map(|certificate| decode_base64(certificate).map(CertificateDer::from))
                    .collect::<Result<_, _>>()?,
            },
            endorsement: json
                .endorsement
                .map(|endorsement| -> Result<Endorsement, MachineProofError> {
                    Ok(Endorsement {
                        client: endorsement.client,
                        public_key: decode_key(&endorsement.public_key)?,
                        signature: decode_base64(&endorsement.signature)?,
                    })
                })
                .transpose()?,
        })
    }
}

fn decode_base64(text: &str) -> Result<Vec<u8>, MachineProofError> {
    BASE64
        .decode(text)
        .map_err(|error| MachineProofError::Decode(format!("not base64: {error}")))
}

fn decode_key(text: &str) -> Result<ClientPublicKey, MachineProofError> {
    ClientPublicKey::from_bytes(&decode_base64(text)?)
        .ok_or_else(|| MachineProofError::Decode("a public key is not 32 bytes".into()))
}

/// The DER of each PEM block labelled `label` in the file at `path`.
fn read_pem(path: &Path, label: &'static str) -> Result<Vec<Vec<u8>>, MachineProofError> {
    let text = std::fs::read(path).map_err(|source| MachineProofError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let blocks: Vec<Vec<u8>> = x509_parser::pem::Pem::iter_from_buffer(&text)
        .filter_map(Result::ok)
        .filter(|pem| pem.label == label)
        .map(|pem| pem.contents)
        .collect();
    if blocks.is_empty() {
        return Err(MachineProofError::NoPem {
            path: path.display().to_string(),
            label,
        });
    }
    Ok(blocks)
}

/// Each certificate in the PEM file at `path`, in order.
fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, MachineProofError> {
    Ok(read_pem(path, "CERTIFICATE")?
        .into_iter()
        .map(CertificateDer::from)
        .collect())
}

#[cfg(test)]
mod tests;
