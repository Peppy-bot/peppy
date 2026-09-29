//! This machine's platform enrollment: the record of the router peer `peppy
//! platform enroll` minted, plus the material the daemon needs to join the
//! project's cloud router. Everything lives under `<root>/conf/peer/`:
//!
//! * `enrollment.json5`: the typed [`EnrollmentDocument`] (`0600`).
//! * `peer.key`: the peer's private key, PKCS#8 PEM (`0600`).
//! * `peer.crt`: the signed leaf certificate alone. The daemon presents it and
//!   nothing above it, so the cloud router closes the link at the leaf's own
//!   expiry.
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
//! version, an id zenoh would refuse, or a missing key file fails the load
//! rather than reaching a router.

use std::path::{Path, PathBuf};

use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use pmi::{ConnectIdentity, RouterId, TlsConfig, ZenohEndpoint, ZenohNetProtocol};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::client::RouterPeerEnrolled;
use crate::error::{Error, Result};
use crate::fs_perms::{restrict_dir, restrict_file};

/// On-disk schema version of `enrollment.json5`. Bumped on any shape change;
/// there is no reader for an older version. A file of another version is
/// rejected by [`load`], and the machine enrolls again.
pub const ENROLLMENT_VERSION: u32 = 2;
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
    /// When this machine received the leaf certificate it holds, from the
    /// enrollment or from the last renewal, unix seconds.
    pub certificate_issued_at: i64,
    /// The leaf certificate's expiry, unix seconds. Past this the cloud router
    /// refuses the link.
    pub certificate_expires_at: i64,
    /// When this machine enrolled, unix seconds.
    pub enrolled_at: i64,
}

impl EnrollmentDocument {
    /// Whether the certificate is at or past expiry at `now_unix`.
    pub fn is_expired(&self, now_unix: i64) -> bool {
        now_unix >= self.certificate_expires_at
    }

    /// When the renewal of the certificate becomes due, unix seconds: after
    /// two thirds of its lifetime. The last third is the time that stays for
    /// a renewal that does not succeed at the first attempt.
    pub fn renewal_due_at(&self) -> i64 {
        let lifetime = (self.certificate_expires_at - self.certificate_issued_at).max(0);
        self.certificate_issued_at + lifetime / 3 * 2
    }

    /// Whether the renewal of the certificate is due at `now_unix`.
    pub fn is_renewal_due(&self, now_unix: i64) -> bool {
        now_unix >= self.renewal_due_at()
    }
}

/// A loaded enrollment: the document plus the absolute paths of the material,
/// each checked to exist at load so the daemon never renders a config naming a
/// file that is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub document: EnrollmentDocument,
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
/// and the PEM material that goes with it. The private key is not part of it,
/// because the platform never holds the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedMaterial {
    pub document: EnrollmentDocument,
    /// The signed leaf alone, as the daemon presents it.
    pub peer_certificate_pem: String,
    pub trust_anchor_pem: String,
    /// The chain above the leaf, kept for inspection.
    pub chain_pem: String,
    pub platform_zenoh_config: String,
}

impl IssuedMaterial {
    /// The material of a new enrollment, from the platform's answer.
    pub fn for_enrollment(
        api_url: &str,
        workspace_id: &str,
        project_id: &str,
        enrolled: RouterPeerEnrolled,
        enrolled_at: i64,
    ) -> Self {
        let document = EnrollmentDocument {
            version: ENROLLMENT_VERSION,
            api_url: api_url.trim_end_matches('/').to_string(),
            workspace_id: workspace_id.to_string(),
            project_id: project_id.to_string(),
            peer_id: enrolled.peer.id,
            peer_name: enrolled.peer.name,
            zenoh_id: enrolled.zenoh_id,
            namespace: enrolled.namespace,
            router: enrolled.address,
            certificate_issued_at: enrolled_at,
            certificate_expires_at: enrolled.peer.certificate_expires_at.timestamp(),
            enrolled_at,
        };
        Self::with_document(
            document,
            enrolled.certificate,
            enrolled.trust_anchor,
            enrolled.chain,
            enrolled.zenoh_config,
        )
    }

    /// The material of a renewal of `enrolled`, from the platform's answer. A
    /// renewal gives a new leaf to the same peer: an answer that names a
    /// different peer, zenoh id, namespace or router address is refused,
    /// because the running daemon cannot take any of them without a restart.
    pub fn for_renewal(
        enrolled: &EnrollmentDocument,
        renewed: RouterPeerEnrolled,
        renewed_at: i64,
    ) -> Result<Self> {
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
            peer_name: renewed.peer.name,
            certificate_issued_at: renewed_at,
            certificate_expires_at: renewed.peer.certificate_expires_at.timestamp(),
            ..enrolled.clone()
        };
        Ok(Self::with_document(
            document,
            renewed.certificate,
            renewed.trust_anchor,
            renewed.chain,
            renewed.zenoh_config,
        ))
    }

    fn with_document(
        document: EnrollmentDocument,
        peer_certificate_pem: String,
        trust_anchor_pem: String,
        chain_pem: String,
        platform_zenoh_config: String,
    ) -> Self {
        Self {
            document,
            peer_certificate_pem,
            trust_anchor_pem,
            chain_pem,
            platform_zenoh_config,
        }
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
/// another version, or missing any of its three PEM files; every message names
/// `peppy platform enroll` as the remedy.
pub fn load(dirs: &PeppyDirs) -> Result<Option<Enrollment>> {
    let path = document_path(dirs);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let document: EnrollmentDocument = serde_json5::from_str(&content).map_err(|e| {
        Error::Auth(format!(
            "failed to parse {}: {e}; run `peppy platform enroll` again",
            path.display()
        ))
    })?;
    if document.version != ENROLLMENT_VERSION {
        return Err(Error::Auth(format!(
            "enrollment file {} is an unsupported format (v{}, expected v{}); \
             run `peppy platform enroll` again",
            path.display(),
            document.version,
            ENROLLMENT_VERSION
        )));
    }
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
    Ok(Some(Enrollment {
        peer_key: material(PEER_KEY_FILE)?,
        peer_certificate: material(PEER_CERTIFICATE_FILE)?,
        trust_anchor: material(TRUST_ANCHOR_FILE)?,
        document,
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

    publish(
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
    publish(
        &peer_dir.join(PEER_CERTIFICATE_FILE),
        &issued.peer_certificate_pem,
        false,
    )?;
    publish(
        &peer_dir.join(TRUST_ANCHOR_FILE),
        &issued.trust_anchor_pem,
        false,
    )?;
    publish(&peer_dir.join(CHAIN_FILE), &issued.chain_pem, false)?;
    publish(
        &peer_dir.join(PLATFORM_ZENOH_CONFIG_FILE),
        &issued.platform_zenoh_config,
        false,
    )?;
    let document = json5_pretty::to_string_pretty(&issued.document)
        .map_err(|e| Error::Auth(format!("failed to serialize the enrollment: {e}")))?;
    publish(&peer_dir.join(ENROLLMENT_FILE), &document, true)
}

fn publish(path: &Path, content: &str, owner_only: bool) -> Result<()> {
    daemon_config::atomic_write::publish_atomic(path, |tmp| {
        std::fs::write(tmp, content)?;
        if owner_only {
            restrict_file(tmp)?;
        }
        Ok(())
    })?;
    Ok(())
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

/// The namespace the enrollment under `dirs` resolves to: the project's when
/// enrolled, `local` otherwise. The same resolution the daemon does at startup,
/// so the CLI can confirm the daemon came back under what it just wrote.
pub fn namespace(dirs: &PeppyDirs) -> Result<Namespace> {
    Ok(load(dirs)?
        .map(|enrollment| enrollment.document.namespace)
        .unwrap_or_else(Namespace::local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RouterPeer;
    use crate::storage::secret;

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";

    const ROUTER_HOST: &str = "rtr-p.us-east-1.robocloud.dev.peppy.bot";
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n";
    const ENROLLED_AT: i64 = 1_700_000_000;
    /// 2027-01-01T00:00:00Z.
    const FIRST_EXPIRY: i64 = 1_798_761_600;

    fn certificate(label: &str) -> String {
        format!("-----BEGIN CERTIFICATE-----\n{label}\n-----END CERTIFICATE-----\n")
    }

    fn enrolled() -> RouterPeerEnrolled {
        RouterPeerEnrolled {
            peer: RouterPeer {
                id: "peer-1".into(),
                name: "robot-7".into(),
                certificate_cn: "robot-7".into(),
                status: "unknown".into(),
                certificate_expires_at: "2027-01-01T00:00:00Z".parse().unwrap(),
                created_at: "2026-10-03T00:00:00Z".parse().unwrap(),
            },
            address: RouterEndpoint::parse(ROUTER_HOST, 7447).unwrap(),
            certificate: certificate("leaf"),
            chain: certificate("issuer"),
            trust_anchor: certificate("ca"),
            zenoh_id: RouterId::parse(ZID).unwrap(),
            namespace: Namespace::parse(PROJECT).unwrap(),
            zenoh_config: "{ mode: \"router\" }".into(),
        }
    }

    /// The answer to a renewal of [`enrolled`]: a new leaf and a later expiry
    /// for the same peer.
    fn renewed() -> RouterPeerEnrolled {
        let mut renewed = enrolled();
        renewed.peer.certificate_expires_at = "2027-04-01T00:00:00Z".parse().unwrap();
        renewed.certificate = certificate("renewed leaf");
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
                ENROLLED_AT,
            ),
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
        assert_eq!(doc.certificate_issued_at, ENROLLED_AT);
        assert_eq!(doc.certificate_expires_at, FIRST_EXPIRY);
        assert_eq!(doc.enrolled_at, ENROLLED_AT);
        assert_eq!(
            issued.peer_certificate_pem,
            certificate("leaf"),
            "the peer presents the leaf alone"
        );
        assert!(issued.chain_pem.contains("issuer"));
    }

    /// A renewal moves the certificate dates and the material, and keeps the
    /// identity and the enrollment date.
    #[test]
    fn the_renewal_material_keeps_the_identity_of_the_enrollment() {
        let enrolled = bundle().issued.document;
        let renewed_at = ENROLLED_AT + 60 * 24 * 60 * 60;

        let issued = IssuedMaterial::for_renewal(&enrolled, renewed(), renewed_at)
            .expect("the same peer renews");

        assert_eq!(
            issued.document,
            EnrollmentDocument {
                certificate_issued_at: renewed_at,
                certificate_expires_at: "2027-04-01T00:00:00Z"
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .unwrap()
                    .timestamp(),
                ..enrolled
            }
        );
        assert_eq!(issued.peer_certificate_pem, certificate("renewed leaf"));
        assert_eq!(issued.trust_anchor_pem, certificate("renewed ca"));
        assert_eq!(issued.chain_pem, certificate("renewed issuer"));
    }

    #[test]
    fn the_renewal_material_takes_the_name_the_platform_shows() {
        let enrolled = bundle().issued.document;
        let mut answer = renewed();
        answer.peer.name = "robot-7 (arm)".into();

        let issued = IssuedMaterial::for_renewal(&enrolled, answer, ENROLLED_AT).unwrap();

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
            let err = IssuedMaterial::for_renewal(&enrolled, answer, ENROLLED_AT)
                .expect_err(member)
                .to_string();
            assert!(err.contains(&format!("a different {member}")), "{err}");
            assert!(err.contains("peppy platform enroll --replace"), "{err}");
        }
    }

    #[test]
    fn the_renewal_is_due_after_two_thirds_of_the_lifetime() {
        let mut doc = bundle().issued.document;
        doc.certificate_issued_at = 1_000;
        doc.certificate_expires_at = 1_000 + 90;
        assert_eq!(doc.renewal_due_at(), 1_060);
        assert!(!doc.is_renewal_due(1_059));
        assert!(doc.is_renewal_due(1_060));

        // A leaf that expires before its issue date is due at once.
        doc.certificate_expires_at = 900;
        assert_eq!(doc.renewal_due_at(), 1_000);
    }

    #[test]
    fn save_then_load_round_trips_and_locks_down_the_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let bundle = bundle();

        save(&dirs, &bundle).expect("save");
        let loaded = load(&dirs).expect("load").expect("enrolled");
        assert_eq!(loaded.document, bundle.issued.document);
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
        assert_eq!(namespace(&dirs).unwrap().as_str(), PROJECT);

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
        assert_eq!(namespace(&dirs).unwrap(), Namespace::local());
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
        let renewed =
            IssuedMaterial::for_renewal(&bundle.issued.document, renewed(), ENROLLED_AT + 5)
                .unwrap();

        save_renewal(&dirs, &renewed).expect("save the renewal");

        let loaded = load(&dirs).expect("load").expect("enrolled");
        assert_eq!(loaded.document, renewed.document);
        let read = |name: &str| std::fs::read_to_string(dirs.peer_dir().join(name)).unwrap();
        assert_eq!(read(PEER_KEY_FILE), KEY_PEM);
        assert_eq!(read(PEER_CERTIFICATE_FILE), certificate("renewed leaf"));
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
        let renewed =
            IssuedMaterial::for_renewal(&bundle().issued.document, renewed(), ENROLLED_AT).unwrap();

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
        assert_eq!(namespace(&dirs).unwrap(), Namespace::local());
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
            ("version", good.replace("version: 2", "version: 1")),
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
        std::fs::remove_file(dirs.peer_dir().join(PEER_KEY_FILE)).unwrap();
        let err = load(&dirs).expect_err("missing key");
        assert!(err.to_string().contains(PEER_KEY_FILE), "{err}");
    }

    #[test]
    fn expiry_is_a_plain_comparison_against_the_given_clock() {
        let doc = bundle().issued.document;
        assert!(!doc.is_expired(doc.certificate_expires_at - 1));
        assert!(doc.is_expired(doc.certificate_expires_at));
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
