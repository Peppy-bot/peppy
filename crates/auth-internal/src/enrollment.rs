//! This machine's platform enrollment: the record of the router peer `peppy
//! platform enroll` minted, plus the material the daemon needs to join the
//! project's cloud router. Everything lives under `<root>/conf/peer/`:
//!
//! * `enrollment.json5`: the typed [`EnrollmentDocument`] (`0600`).
//! * `peer.key`: the peer's private key, PKCS#8 PEM (`0600`).
//! * `peer.crt`: the signed leaf certificate alone. The daemon presents it and
//!   nothing above it, so the cloud router closes the link at the leaf's own
//!   expiry. Its validity is what the renewal is scheduled from.
//! * `ca.crt`: the project CA the cloud router's certificate chains to.
//! * `chain.crt`: the leaf's issuing chain, for `openssl verify`. The daemon
//!   does not read it.
//! * `zenohd.platform.json5`: the peer config the platform rendered, kept for
//!   inspection only. The daemon renders its own config from the document.
//!
//! The daemon reads the directory at startup and on every control-socket poke;
//! the CLI writes it on `enroll` and removes it on `unenroll`. A renewal
//! ([`crate::renewal`]) replaces the certificates and the document, and leaves
//! the key as it is. An absent `enrollment.json5` means "not enrolled", and a
//! present one is parsed in full before anything uses it: an unsupported
//! version, an id zenoh would refuse, a missing key file, or a leaf that is not
//! a certificate fails the load rather than reaching a router.

use std::path::{Path, PathBuf};

use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use pmi::{ConnectIdentity, RouterId, TlsConfig, ZenohEndpoint, ZenohNetProtocol};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::client::RouterPeerEnrolled;
use crate::document::{self, Versioned};
use crate::error::{Error, Result};
use crate::fs_perms::restrict_dir;

/// On-disk schema version of `enrollment.json5`. There is one reader, for
/// this version. A file of another version is rejected by [`load`], and the
/// machine enrolls again.
pub const ENROLLMENT_VERSION: u32 = 1;
pub const ENROLLMENT_FILE: &str = "enrollment.json5";
pub const PEER_KEY_FILE: &str = "peer.key";
pub const PEER_CERTIFICATE_FILE: &str = "peer.crt";
pub const TRUST_ANCHOR_FILE: &str = "ca.crt";
pub const CHAIN_FILE: &str = "chain.crt";
pub const PLATFORM_ZENOH_CONFIG_FILE: &str = "zenohd.platform.json5";

/// Where the enrolled machine's router dials: the project's cloud router.
///
/// Constructed only through [`RouterEndpoint::parse`], which runs the pair
/// through pmi's locator parser, the single source of what a `tls/` endpoint
/// may look like, so [`RouterEndpoint::locator`] can never fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRouterEndpoint", into = "RawRouterEndpoint")]
pub struct RouterEndpoint {
    host: String,
    port: u16,
}

#[derive(Serialize, Deserialize)]
struct RawRouterEndpoint {
    host: String,
    port: u16,
}

impl RouterEndpoint {
    pub fn parse(host: &str, port: u16) -> Result<Self> {
        let locator = format!("{}/{host}:{port}", ZenohNetProtocol::Tls);
        let endpoint: ZenohEndpoint = locator
            .parse()
            .map_err(|e| Error::Auth(format!("invalid router endpoint {locator:?}: {e}")))?;
        Ok(Self {
            host: endpoint.host().to_string(),
            port: endpoint.port(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `tls/<host>:<port>` locator the router's `connect` block dials.
    pub fn locator(&self) -> String {
        format!("{}/{}:{}", ZenohNetProtocol::Tls, self.host, self.port)
    }
}

impl TryFrom<RawRouterEndpoint> for RouterEndpoint {
    type Error = Error;

    fn try_from(raw: RawRouterEndpoint) -> Result<Self> {
        Self::parse(&raw.host, raw.port)
    }
}

impl From<RouterEndpoint> for RawRouterEndpoint {
    fn from(endpoint: RouterEndpoint) -> Self {
        Self {
            host: endpoint.host,
            port: endpoint.port,
        }
    }
}

/// The persisted enrollment record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentDocument {
    /// Schema version (see [`ENROLLMENT_VERSION`]). Defaults to `0` when absent
    /// so an unversioned file is rejected rather than half-interpreted.
    #[serde(default)]
    pub version: u32,
    /// The platform API the peer was enrolled against.
    pub api_url: String,
    pub workspace_id: String,
    pub project_id: String,
    /// The platform's id for this peer, the handle `unenroll` deletes.
    pub peer_id: String,
    /// The certificate's common name, and the peer's display name.
    pub peer_name: String,
    /// The transport identity the daemon's router runs under. The platform
    /// marks the peer connected only while a session with exactly this id is
    /// present, so the daemon never mints its own while enrolled.
    pub zenoh_id: RouterId,
    /// The project's routing namespace, applied to every application session
    /// the daemon and its nodes open.
    pub namespace: Namespace,
    pub router: RouterEndpoint,
    /// When this machine enrolled, unix seconds.
    pub enrolled_at: i64,
}

/// The validity of the peer's leaf certificate as the certificate states it,
/// unix seconds. The cloud router refuses the link past `not_after`, so the
/// renewal is scheduled from these two dates and from nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateValidity {
    pub not_before: i64,
    pub not_after: i64,
}

impl CertificateValidity {
    /// The validity of the first certificate of `pem`.
    pub fn parse_pem(pem: &str) -> Result<Self> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
            .map_err(|e| Error::Auth(format!("the peer certificate is not PEM: {e}")))?;
        let certificate = pem
            .parse_x509()
            .map_err(|e| Error::Auth(format!("the peer certificate is not X.509: {e}")))?;
        let validity = certificate.validity();
        Ok(Self {
            not_before: validity.not_before.timestamp(),
            not_after: validity.not_after.timestamp(),
        })
    }

    /// Whether the certificate is at or past expiry at `now_unix`.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.not_after
    }

    /// When the renewal of the certificate becomes due, unix seconds: after
    /// two thirds of its lifetime. The last third is the time that stays for
    /// a renewal that does not succeed at the first attempt.
    pub fn renewal_due_at(&self) -> i64 {
        let lifetime = (self.not_after - self.not_before).max(0);
        self.not_before + lifetime / 3 * 2
    }

    /// Whether the renewal of the certificate is due at `now_unix`.
    pub fn is_renewal_due(&self, now_unix: i64) -> bool {
        now_unix >= self.renewal_due_at()
    }
}

impl Versioned for EnrollmentDocument {
    const VERSION: u32 = ENROLLMENT_VERSION;
    const WHAT: &'static str = "enrollment";
    const REMEDY: &'static str = "peppy platform enroll";

    fn version(&self) -> u32 {
        self.version
    }
}

/// A loaded enrollment: the document, the validity of the leaf, and the
/// absolute paths of the material, each checked to exist at load so the
/// daemon never renders a config naming a file that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub document: EnrollmentDocument,
    /// The validity of the leaf in `peer_certificate`.
    pub certificate: CertificateValidity,
    pub peer_key: PathBuf,
    pub peer_certificate: PathBuf,
    pub trust_anchor: PathBuf,
}

impl Enrollment {
    /// The upstream the daemon's router federates to: the cloud router's
    /// locator and the mutual-TLS material to dial it with.
    pub fn federation_target(&self) -> (String, TlsConfig) {
        (
            self.document.router.locator(),
            TlsConfig::mtls_client(
                self.trust_anchor.clone(),
                ConnectIdentity {
                    certificate: self.peer_certificate.clone(),
                    private_key: self.peer_key.clone(),
                },
            ),
        )
    }
}

/// What the platform issues at an enrollment and at each renewal: the document
/// and the PEM material that goes with it, with the validity read from the
/// leaf. The private key is not part of it, because the platform never holds
/// the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedMaterial {
    pub document: EnrollmentDocument,
    /// The validity of the leaf in `peer_certificate_pem`.
    pub certificate: CertificateValidity,
    /// The signed leaf alone, as the daemon presents it.
    pub peer_certificate_pem: String,
    pub trust_anchor_pem: String,
    /// The chain above the leaf, kept for inspection.
    pub chain_pem: String,
    pub platform_zenoh_config: String,
}

impl IssuedMaterial {
    /// The material of a new enrollment, from the platform's answer. A leaf
    /// that is not a certificate is refused before anything is written.
    pub fn for_enrollment(
        api_url: &str,
        workspace_id: &str,
        project_id: &str,
        enrolled: RouterPeerEnrolled,
        enrolled_at: i64,
    ) -> Result<Self> {
        let document = EnrollmentDocument {
            version: ENROLLMENT_VERSION,
            api_url: api_url.trim_end_matches('/').to_string(),
            workspace_id: workspace_id.to_string(),
            project_id: project_id.to_string(),
            peer_id: enrolled.peer.id.clone(),
            peer_name: enrolled.peer.name.clone(),
            zenoh_id: enrolled.zenoh_id.clone(),
            namespace: enrolled.namespace.clone(),
            router: enrolled.address.clone(),
            enrolled_at,
        };
        Self::with_document(document, enrolled)
    }

    /// The material of a renewal of `enrolled`, from the platform's answer. A
    /// renewal gives a new leaf to the same peer: an answer that names a
    /// different peer, zenoh id, namespace or router address is refused,
    /// because the running daemon cannot take any of them without a restart.
    pub fn for_renewal(enrolled: &EnrollmentDocument, renewed: RouterPeerEnrolled) -> Result<Self> {
        let differences = [
            ("peer id", renewed.peer.id != enrolled.peer_id),
            ("zenoh id", renewed.zenoh_id != enrolled.zenoh_id),
            ("namespace", renewed.namespace != enrolled.namespace),
            ("router address", renewed.address != enrolled.router),
        ];
        if let Some((member, _)) = differences.iter().find(|(_, differs)| *differs) {
            return Err(Error::Auth(format!(
                "the platform answered the renewal of peer {} with a different {member}; \
                 nothing was written; run `peppy platform enroll --replace`",
                enrolled.peer_id
            )));
        }
        let document = EnrollmentDocument {
            peer_name: renewed.peer.name.clone(),
            ..enrolled.clone()
        };
        Self::with_document(document, renewed)
    }

    fn with_document(document: EnrollmentDocument, issued: RouterPeerEnrolled) -> Result<Self> {
        Ok(Self {
            document,
            certificate: CertificateValidity::parse_pem(&issued.certificate)?,
            peer_certificate_pem: issued.certificate,
            trust_anchor_pem: issued.trust_anchor,
            chain_pem: issued.chain,
            platform_zenoh_config: issued.zenoh_config,
        })
    }
}

/// Everything `enroll` writes: the private key this machine made, and what the
/// platform issued for it.
pub struct EnrollmentBundle {
    pub peer_key_pem: SecretString,
    pub issued: IssuedMaterial,
}

fn document_path(dirs: &PeppyDirs) -> PathBuf {
    dirs.peer_dir().join(ENROLLMENT_FILE)
}

/// Loads this machine's enrollment. `Ok(None)` when `enrollment.json5` is
/// absent (not enrolled). An error when it is present but unreadable, of
/// another version, missing any of its three PEM files, or with a leaf that is
/// not a certificate; every message names `peppy platform enroll` as the
/// remedy.
pub fn load(dirs: &PeppyDirs) -> Result<Option<Enrollment>> {
    let Some(document) = document::load::<EnrollmentDocument>(&document_path(dirs))? else {
        return Ok(None);
    };
    let peer_dir = dirs.peer_dir();
    let material = |name: &str| -> Result<PathBuf> {
        let file = peer_dir.join(name);
        if !file.is_file() {
            return Err(Error::Auth(format!(
                "enrollment material {} is missing; run `peppy platform enroll` again",
                file.display()
            )));
        }
        Ok(file)
    };
    let peer_certificate = material(PEER_CERTIFICATE_FILE)?;
    let certificate = CertificateValidity::parse_pem(&std::fs::read_to_string(&peer_certificate)?)
        .map_err(|e| {
            Error::Auth(format!(
                "enrollment material {} cannot be read: {e}; run `peppy platform enroll` again",
                peer_certificate.display()
            ))
        })?;
    Ok(Some(Enrollment {
        peer_key: material(PEER_KEY_FILE)?,
        peer_certificate,
        trust_anchor: material(TRUST_ANCHOR_FILE)?,
        document,
        certificate,
    }))
}

/// Writes the bundle under `<root>/conf/peer/` (`0700`), the key and the
/// document owner-only. Each file is published atomically, and the document
/// last: a crash before it lands leaves no record, so the machine reads as
/// not enrolled and the next `enroll` overwrites the partial material.
pub fn save(dirs: &PeppyDirs, bundle: &EnrollmentBundle) -> Result<()> {
    let peer_dir = dirs.peer_dir();
    std::fs::create_dir_all(&peer_dir)?;
    restrict_dir(&peer_dir)?;
    // `conf/` holds the credentials too; keep it owner-only as `storage` does.
    if let Some(conf) = peer_dir.parent() {
        restrict_dir(conf)?;
    }

    document::publish(
        &peer_dir.join(PEER_KEY_FILE),
        bundle.peer_key_pem.expose_secret(),
        true,
    )?;
    publish_issued(&peer_dir, &bundle.issued)
}

/// Writes the material of a renewal over the material of the enrollment. The
/// key stays as it is. A renewal has no key of its own, so when the key of the
/// enrollment is absent the machine is not enrolled, and nothing is written.
pub fn save_renewal(dirs: &PeppyDirs, renewed: &IssuedMaterial) -> Result<()> {
    let peer_dir = dirs.peer_dir();
    let peer_key = peer_dir.join(PEER_KEY_FILE);
    if !peer_key.is_file() {
        return Err(Error::Auth(format!(
            "enrollment material {} is missing, so the renewed certificate was not written; \
             run `peppy platform enroll` again",
            peer_key.display()
        )));
    }
    publish_issued(&peer_dir, renewed)
}

/// Publishes what the platform issued, the document last: a crash before it
/// lands leaves the document of the material that was there before.
fn publish_issued(peer_dir: &Path, issued: &IssuedMaterial) -> Result<()> {
    for (file, content) in [
        (PEER_CERTIFICATE_FILE, &issued.peer_certificate_pem),
        (TRUST_ANCHOR_FILE, &issued.trust_anchor_pem),
        (CHAIN_FILE, &issued.chain_pem),
        (PLATFORM_ZENOH_CONFIG_FILE, &issued.platform_zenoh_config),
    ] {
        document::publish(&peer_dir.join(file), content, false)?;
    }
    document::save(&peer_dir.join(ENROLLMENT_FILE), &issued.document, true)
}

/// Removes the whole peer directory. Absent is not an error: the machine is
/// then simply not enrolled.
pub fn remove(dirs: &PeppyDirs) -> Result<()> {
    match std::fs::remove_dir_all(dirs.peer_dir()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// The identity an enrollment prescribes for a daemon generation: the
/// namespace its sessions open with and, when enrolled, the platform-minted id
/// its router pins. The daemon compares the identity of the enrollment on disk
/// with the one it booted under to decide whether a poke needs a generation
/// restart, and the CLI compares it with the daemon's recorded state to
/// confirm that the daemon came back under what it just wrote. An unenrolled
/// generation runs a per-boot router id that nothing on disk names, so it
/// carries `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationIdentity {
    pub namespace: Namespace,
    pub router_id: Option<RouterId>,
}

impl FederationIdentity {
    /// The identity `enrollment` prescribes: the project namespace and zenoh
    /// id when enrolled, `local` and no pinned id otherwise.
    pub fn of(enrollment: Option<&Enrollment>) -> Self {
        match enrollment {
            Some(enrollment) => Self {
                namespace: enrollment.document.namespace.clone(),
                router_id: Some(enrollment.document.zenoh_id.clone()),
            },
            None => Self {
                namespace: Namespace::local(),
                router_id: None,
            },
        }
    }

    /// The identity the enrollment under `dirs` prescribes.
    pub fn on_disk(dirs: &PeppyDirs) -> Result<Self> {
        Ok(Self::of(load(dirs)?.as_ref()))
    }

    /// Whether a generation that runs under `namespace` and `router_id` runs
    /// under this identity. An unenrolled identity pins no router id, so the
    /// per-boot id of such a generation is not compared.
    pub fn is_run_by(&self, namespace: &Namespace, router_id: Option<&RouterId>) -> bool {
        self.namespace == *namespace
            && self
                .router_id
                .as_ref()
                .is_none_or(|expected| router_id == Some(expected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::PeerStatus;
    use crate::storage::secret;
    use crate::test_support::{EXPIRES_AT, ISSUED_AT, PROJECT, ZID, leaf_pem, router_peer};

    const ROUTER_HOST: &str = "rtr-p.us-east-1.robocloud.dev.peppy.bot";
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n";
    const DAY: i64 = 24 * 60 * 60;

    fn certificate(label: &str) -> String {
        format!("-----BEGIN CERTIFICATE-----\n{label}\n-----END CERTIFICATE-----\n")
    }

    /// The answer to an enrollment: a leaf valid from [`ISSUED_AT`] to
    /// [`EXPIRES_AT`].
    fn enrolled() -> RouterPeerEnrolled {
        RouterPeerEnrolled {
            peer: router_peer("peer-1", "robot-7", PeerStatus::Unknown),
            address: RouterEndpoint::parse(ROUTER_HOST, 7447).unwrap(),
            certificate: leaf_pem(ISSUED_AT, EXPIRES_AT),
            chain: certificate("issuer"),
            trust_anchor: certificate("ca"),
            zenoh_id: RouterId::parse(ZID).unwrap(),
            namespace: Namespace::parse(PROJECT).unwrap(),
            zenoh_config: "{ mode: \"router\" }".into(),
        }
    }

    /// The answer to a renewal of [`enrolled`]: a new leaf, issued 60 days
    /// after the first, for the same peer.
    fn renewed() -> RouterPeerEnrolled {
        let mut renewed = enrolled();
        renewed.certificate = leaf_pem(ISSUED_AT + 60 * DAY, EXPIRES_AT + 60 * DAY);
        renewed.chain = certificate("renewed issuer");
        renewed.trust_anchor = certificate("renewed ca");
        renewed.zenoh_config = "{ mode: \"router\", renewed: true }".into();
        renewed
    }

    fn bundle() -> EnrollmentBundle {
        EnrollmentBundle {
            peer_key_pem: secret(KEY_PEM.into()),
            issued: IssuedMaterial::for_enrollment(
                "https://api.example.test/",
                "ws-1",
                PROJECT,
                enrolled(),
                ISSUED_AT,
            )
            .expect("a certificate"),
        }
    }

    #[test]
    fn the_enrollment_material_is_built_from_the_platform_response() {
        let issued = bundle().issued;
        let doc = &issued.document;
        assert_eq!(doc.version, ENROLLMENT_VERSION);
        assert_eq!(doc.api_url, "https://api.example.test");
        assert_eq!(doc.workspace_id, "ws-1");
        assert_eq!(doc.project_id, PROJECT);
        assert_eq!(doc.peer_id, "peer-1");
        assert_eq!(doc.peer_name, "robot-7");
        assert_eq!(doc.zenoh_id.as_str(), ZID);
        assert_eq!(doc.namespace.as_str(), PROJECT);
        assert_eq!(doc.router.host(), ROUTER_HOST);
        assert_eq!(doc.router.port(), 7447);
        assert_eq!(
            doc.router.locator(),
            "tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447"
        );
        assert_eq!(doc.enrolled_at, ISSUED_AT);
        assert_eq!(
            issued.certificate,
            CertificateValidity {
                not_before: ISSUED_AT,
                not_after: EXPIRES_AT,
            },
            "the validity is read from the leaf"
        );
        assert!(
            issued
                .peer_certificate_pem
                .starts_with("-----BEGIN CERTIFICATE-----"),
            "the peer presents the leaf alone"
        );
        assert!(issued.chain_pem.contains("issuer"));
    }

    /// A leaf that is not a certificate is refused before anything is written.
    #[test]
    fn an_answer_whose_leaf_is_not_a_certificate_is_refused() {
        let mut answer = enrolled();
        answer.certificate = certificate("leaf");
        let err = IssuedMaterial::for_enrollment("https://api.example", "ws-1", PROJECT, answer, 0)
            .expect_err("not a certificate");
        assert!(err.to_string().contains("peer certificate"), "{err}");
    }

    /// A renewal moves the certificate and its validity, and keeps the
    /// identity and the enrollment date.
    #[test]
    fn the_renewal_material_keeps_the_identity_of_the_enrollment() {
        let enrolled = bundle().issued.document;
        let answer = renewed();

        let issued =
            IssuedMaterial::for_renewal(&enrolled, answer.clone()).expect("the same peer renews");

        assert_eq!(issued.document, enrolled);
        assert_eq!(
            issued.certificate,
            CertificateValidity {
                not_before: ISSUED_AT + 60 * DAY,
                not_after: EXPIRES_AT + 60 * DAY,
            }
        );
        assert_eq!(issued.peer_certificate_pem, answer.certificate);
        assert_eq!(issued.trust_anchor_pem, certificate("renewed ca"));
        assert_eq!(issued.chain_pem, certificate("renewed issuer"));
    }

    #[test]
    fn the_renewal_material_takes_the_name_the_platform_shows() {
        let enrolled = bundle().issued.document;
        let mut answer = renewed();
        answer.peer.name = "robot-7 (arm)".into();

        let issued = IssuedMaterial::for_renewal(&enrolled, answer).unwrap();

        assert_eq!(issued.document.peer_name, "robot-7 (arm)");
    }

    /// An answer that names a different identity is refused, and the message
    /// names the member and the remedy.
    #[test]
    fn a_renewal_answer_with_a_different_identity_is_refused() {
        let enrolled = bundle().issued.document;
        type Change = fn(&mut RouterPeerEnrolled);
        let cases: [(&str, Change); 4] = [
            ("peer id", |answer| answer.peer.id = "peer-2".into()),
            ("zenoh id", |answer| {
                answer.zenoh_id = RouterId::parse("7f3a9c1e").unwrap()
            }),
            ("namespace", |answer| {
                answer.namespace = Namespace::parse("11111111-e29b-41d4-a716-446655440000").unwrap()
            }),
            ("router address", |answer| {
                answer.address = RouterEndpoint::parse("rtr-other.example", 7447).unwrap()
            }),
        ];
        for (member, change) in cases {
            let mut answer = renewed();
            change(&mut answer);
            let err = IssuedMaterial::for_renewal(&enrolled, answer)
                .expect_err(member)
                .to_string();
            assert!(err.contains(&format!("a different {member}")), "{err}");
            assert!(err.contains("peppy platform enroll --replace"), "{err}");
        }
    }

    #[test]
    fn the_renewal_is_due_after_two_thirds_of_the_lifetime() {
        let validity = CertificateValidity {
            not_before: 1_000,
            not_after: 1_000 + 90,
        };
        assert_eq!(validity.renewal_due_at(), 1_060);
        assert!(!validity.is_renewal_due(1_059));
        assert!(validity.is_renewal_due(1_060));

        // A leaf that expires before its issue date is due at once.
        let inverted = CertificateValidity {
            not_after: 900,
            ..validity
        };
        assert_eq!(inverted.renewal_due_at(), 1_000);
    }

    #[test]
    fn save_then_load_round_trips_and_locks_down_the_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let bundle = bundle();

        save(&dirs, &bundle).expect("save");
        let loaded = load(&dirs).expect("load").expect("enrolled");
        assert_eq!(loaded.document, bundle.issued.document);
        assert_eq!(
            loaded.certificate, bundle.issued.certificate,
            "the validity is read from the leaf on disk"
        );
        assert_eq!(loaded.peer_key, dirs.peer_dir().join(PEER_KEY_FILE));
        assert_eq!(
            std::fs::read_to_string(&loaded.peer_certificate).unwrap(),
            bundle.issued.peer_certificate_pem
        );
        assert_eq!(
            std::fs::read_to_string(&loaded.trust_anchor).unwrap(),
            bundle.issued.trust_anchor_pem
        );
        assert_eq!(
            std::fs::read_to_string(dirs.peer_dir().join(CHAIN_FILE)).unwrap(),
            bundle.issued.chain_pem
        );
        assert_eq!(
            std::fs::read_to_string(dirs.peer_dir().join(PLATFORM_ZENOH_CONFIG_FILE)).unwrap(),
            bundle.issued.platform_zenoh_config
        );
        assert_eq!(
            FederationIdentity::on_disk(&dirs).unwrap(),
            FederationIdentity {
                namespace: Namespace::parse(PROJECT).unwrap(),
                router_id: Some(RouterId::parse(ZID).unwrap()),
            }
        );

        let (locator, tls) = loaded.federation_target();
        assert_eq!(locator, "tls/rtr-p.us-east-1.robocloud.dev.peppy.bot:7447");
        assert_eq!(tls.root_ca_certificate, Some(loaded.trust_anchor.clone()));
        assert_eq!(
            tls.connect_identity,
            Some(ConnectIdentity {
                certificate: loaded.peer_certificate.clone(),
                private_key: loaded.peer_key.clone(),
            })
        );
        assert!(tls.verify_name_on_connect);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dirs.peer_dir()), 0o700);
            assert_eq!(mode(&loaded.peer_key), 0o600);
            assert_eq!(mode(&dirs.peer_dir().join(ENROLLMENT_FILE)), 0o600);
        }

        remove(&dirs).expect("remove");
        assert!(load(&dirs).unwrap().is_none());
        remove(&dirs).expect("removing twice is fine");
    }

    /// A renewal replaces the certificates and the document. The key file is
    /// not touched, and it stays owner-only.
    #[test]
    fn a_saved_renewal_replaces_the_certificates_and_keeps_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let bundle = bundle();
        save(&dirs, &bundle).expect("save");
        let renewed = IssuedMaterial::for_renewal(&bundle.issued.document, renewed()).unwrap();

        save_renewal(&dirs, &renewed).expect("save the renewal");

        let loaded = load(&dirs).expect("load").expect("enrolled");
        assert_eq!(loaded.document, renewed.document);
        assert_eq!(loaded.certificate, renewed.certificate);
        let read = |name: &str| std::fs::read_to_string(dirs.peer_dir().join(name)).unwrap();
        assert_eq!(read(PEER_KEY_FILE), KEY_PEM);
        assert_eq!(read(PEER_CERTIFICATE_FILE), renewed.peer_certificate_pem);
        assert_eq!(read(TRUST_ANCHOR_FILE), certificate("renewed ca"));
        assert_eq!(read(CHAIN_FILE), certificate("renewed issuer"));
        assert_eq!(
            read(PLATFORM_ZENOH_CONFIG_FILE),
            renewed.platform_zenoh_config
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&loaded.peer_key), 0o600);
            assert_eq!(mode(&dirs.peer_dir().join(ENROLLMENT_FILE)), 0o600);
        }
    }

    /// A machine that is not enrolled has no key, so a renewal writes nothing
    /// and does not make the peer directory.
    #[test]
    fn a_renewal_writes_nothing_on_a_machine_that_is_not_enrolled() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let renewed = IssuedMaterial::for_renewal(&bundle().issued.document, renewed()).unwrap();

        let err = save_renewal(&dirs, &renewed).expect_err("not enrolled");

        assert!(err.to_string().contains("peppy platform enroll"), "{err}");
        assert!(!dirs.peer_dir().exists());
        assert!(load(&dirs).unwrap().is_none());
    }

    #[test]
    fn an_absent_enrollment_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        assert!(load(&dirs).unwrap().is_none());
        assert_eq!(
            FederationIdentity::on_disk(&dirs).unwrap(),
            FederationIdentity::of(None)
        );
    }

    /// The identity of an unenrolled generation is `local` with no pinned id;
    /// a generation matches it under any per-boot id. An enrolled identity is
    /// matched by its namespace and its zenoh id together.
    #[test]
    fn the_identity_follows_the_enrollment_and_pins_the_id_only_when_enrolled() {
        let local = Namespace::local();
        let project = Namespace::parse(PROJECT).unwrap();
        let zid = RouterId::parse(ZID).unwrap();
        let other = RouterId::parse("7f3a").unwrap();

        let unenrolled = FederationIdentity::of(None);
        assert_eq!(
            unenrolled,
            FederationIdentity {
                namespace: local.clone(),
                router_id: None
            }
        );
        assert!(unenrolled.is_run_by(&local, Some(&other)));
        assert!(unenrolled.is_run_by(&local, None));
        assert!(!unenrolled.is_run_by(&project, None));

        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        save(&dirs, &bundle()).expect("save");
        let enrolled = FederationIdentity::on_disk(&dirs).unwrap();
        assert_eq!(
            enrolled,
            FederationIdentity {
                namespace: project.clone(),
                router_id: Some(zid.clone())
            }
        );
        assert!(enrolled.is_run_by(&project, Some(&zid)));
        assert!(
            !enrolled.is_run_by(&project, Some(&other)),
            "the old generation under the same project but another id is not back yet"
        );
        assert!(!enrolled.is_run_by(&local, Some(&zid)));
        assert!(!enrolled.is_run_by(&project, None));
    }

    /// Every way the directory can be present but unusable fails the load with
    /// the remedy named, rather than reaching the daemon's router.
    #[test]
    fn a_broken_enrollment_fails_the_load_naming_enroll() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        save(&dirs, &bundle()).unwrap();
        let document_file = dirs.peer_dir().join(ENROLLMENT_FILE);
        let good = std::fs::read_to_string(&document_file).unwrap();

        for (label, content) in [
            ("version", good.replace("version: 1", "version: 2")),
            ("unversioned", good.replace("version: 1,", "")),
            ("leading-zero zid", good.replace(ZID, "0abc")),
            ("uppercase zid", good.replace(ZID, "ABC")),
            ("namespace", good.replace(PROJECT, "**")),
            ("host", good.replace(ROUTER_HOST, "")),
            ("syntax", "{ not json5".to_string()),
        ] {
            std::fs::write(&document_file, content).unwrap();
            let err = load(&dirs).expect_err(label);
            assert!(
                err.to_string().contains("peppy platform enroll"),
                "{label}: {err}"
            );
        }

        std::fs::write(&document_file, &good).unwrap();
        let leaf_file = dirs.peer_dir().join(PEER_CERTIFICATE_FILE);
        std::fs::write(&leaf_file, certificate("not x509")).unwrap();
        let err = load(&dirs).expect_err("a leaf that is not a certificate");
        assert!(err.to_string().contains(PEER_CERTIFICATE_FILE), "{err}");
        assert!(err.to_string().contains("peppy platform enroll"), "{err}");

        std::fs::write(&leaf_file, leaf_pem(ISSUED_AT, EXPIRES_AT)).unwrap();
        std::fs::remove_file(dirs.peer_dir().join(PEER_KEY_FILE)).unwrap();
        let err = load(&dirs).expect_err("missing key");
        assert!(err.to_string().contains(PEER_KEY_FILE), "{err}");
    }

    #[test]
    fn expiry_is_a_plain_comparison_against_the_given_clock() {
        let validity = bundle().issued.certificate;
        assert!(!validity.is_expired(validity.not_after - 1));
        assert!(validity.is_expired(validity.not_after));
    }

    #[test]
    fn router_endpoints_are_validated_on_read() {
        assert!(RouterEndpoint::parse("", 7447).is_err());
        assert!(serde_json::from_str::<RouterEndpoint>(r#"{"host":"","port":1}"#).is_err());
        let endpoint: RouterEndpoint =
            serde_json::from_str(r#"{"host":"rtr.example","port":7447}"#).unwrap();
        assert_eq!(endpoint.locator(), "tls/rtr.example:7447");
    }
}
