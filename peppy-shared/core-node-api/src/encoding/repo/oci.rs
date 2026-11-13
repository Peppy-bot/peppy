//! The names an OCI registry holds container nodes under: the repository
//! (`oci://<registry host>/<repository path>`), the platform of an artifact,
//! and the tag of one artifact (`<node name>.<node tag>.<peppy version>.linux-<arch>`).
//!
//! Each type is parsed once, where the text enters peppy (an entry of
//! `repositories.json5`, `publish_repository` in `peppy_config.json5`, a tag
//! that a registry lists), so the code that holds one can never meet a value
//! that a registry would refuse.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The scheme of a repository URL that names an OCI repository.
pub const OCI_SCHEME: &str = "oci";

/// The longest tag a registry accepts (OCI distribution specification).
pub const MAX_ARTIFACT_TAG_LENGTH: usize = 128;

/// The longest `<registry host>/<repository path>` a registry accepts.
const MAX_REPOSITORY_NAME_LENGTH: usize = 255;

/// One OCI repository: `oci://<registry host>/<repository path>`, for example
/// `oci://ghcr.io/acme/peppy-nodes`.
///
/// The registry host is lowercase and may carry a port. The repository path
/// follows the rules of the OCI distribution specification: lowercase
/// components of letters and digits, joined inside a component by `.`, `_`,
/// `__` or a run of `-`, and separated by `/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OciRepository {
    /// `host` or `host:port`, lowercase. An IPv6 host keeps its brackets.
    registry: String,
    /// The repository path, without a leading or trailing `/`.
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OciRepositoryError {
    #[error("`{0}` is not a URL: expected oci://<registry host>/<repository path>")]
    NotAUrl(String),
    #[error("`{0}` does not start with oci://: expected oci://<registry host>/<repository path>")]
    NotOciScheme(String),
    #[error("`{0}` names no registry host: expected oci://<registry host>/<repository path>")]
    NoHost(String),
    #[error("`{0}` names no repository path: expected oci://<registry host>/<repository path>")]
    NoPath(String),
    /// The URL stays out of the message: it holds the credentials.
    #[error(
        "the URL holds credentials: peppy reads registry credentials from the Docker style auth \
         file, never from the URL"
    )]
    Credentials,
    #[error("`{0}` has a query or a fragment, which an OCI repository URL does not have")]
    QueryOrFragment(String),
    #[error(
        "`{url}` has the repository path component `{component}`, which an OCI registry refuses: \
         a component holds lowercase letters and digits, joined by `.`, `_`, `__` or `-`, and \
         starts and ends with a letter or a digit"
    )]
    InvalidPathComponent { url: String, component: String },
    #[error(
        "`{url}` is {length} characters from the registry host to the end of the path; a \
         registry accepts at most {MAX_REPOSITORY_NAME_LENGTH}"
    )]
    TooLong { url: String, length: usize },
}

impl OciRepository {
    /// Parses `oci://<registry host>[:<port>]/<repository path>`.
    pub fn parse(raw: &str) -> Result<Self, OciRepositoryError> {
        let text = raw.trim();
        let url =
            url::Url::parse(text).map_err(|_| OciRepositoryError::NotAUrl(text.to_owned()))?;
        if url.scheme() != OCI_SCHEME {
            return Err(OciRepositoryError::NotOciScheme(text.to_owned()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OciRepositoryError::Credentials);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(OciRepositoryError::QueryOrFragment(text.to_owned()));
        }
        let host = match url.host() {
            Some(url::Host::Domain(domain)) if !domain.is_empty() => domain.to_ascii_lowercase(),
            Some(url::Host::Ipv4(address)) => address.to_string(),
            Some(url::Host::Ipv6(address)) => format!("[{address}]"),
            _ => return Err(OciRepositoryError::NoHost(text.to_owned())),
        };
        let registry = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        // The path as written: the URL parser percent-encodes characters
        // that a path component may not hold, and the refusal must quote
        // what the user wrote. The authority holds no `/`, and a query or a
        // fragment is refused above.
        let path = text
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map_or("", |(_, path)| path)
            .trim_matches('/');
        if path.is_empty() {
            return Err(OciRepositoryError::NoPath(text.to_owned()));
        }
        for component in path.split('/') {
            if !is_repository_path_component(component) {
                return Err(OciRepositoryError::InvalidPathComponent {
                    url: text.to_owned(),
                    component: component.to_owned(),
                });
            }
        }
        let length = registry.len() + 1 + path.len();
        if length > MAX_REPOSITORY_NAME_LENGTH {
            return Err(OciRepositoryError::TooLong {
                url: text.to_owned(),
                length,
            });
        }
        Ok(Self {
            registry,
            path: path.to_owned(),
        })
    }

    /// `host` or `host:port`, the key the Docker style auth file names the
    /// registry by.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The registry host without its port.
    pub fn registry_host(&self) -> &str {
        match self.registry.rsplit_once(':') {
            // An IPv6 host is bracketed, so a `:` inside the brackets is
            // part of the address and not a port separator.
            Some((host, port)) if !port.contains(']') => host,
            _ => &self.registry,
        }
    }

    /// The repository path, without a leading or trailing `/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the registry host is this machine (`localhost`, a `127.0.0.0/8`
    /// address or `::1`). Peppy reaches such a registry over plain HTTP, as
    /// Docker does, and every other registry over HTTPS.
    pub fn registry_is_loopback(&self) -> bool {
        let host = self.registry_host();
        if host == "localhost" {
            return true;
        }
        if let Ok(address) = host.parse::<Ipv4Addr>() {
            return address.is_loopback();
        }
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback())
    }
}

impl fmt::Display for OciRepository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{OCI_SCHEME}://{}/{}", self.registry, self.path)
    }
}

impl serde::Serialize for OciRepository {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for OciRepository {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// One component of a repository path, as the OCI distribution
/// specification defines it:
/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn is_repository_path_component(component: &str) -> bool {
    let is_alphanumeric = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = component.chars().peekable();
    let mut expect_alphanumeric = true;
    while let Some(c) = chars.next() {
        if is_alphanumeric(c) {
            expect_alphanumeric = false;
            continue;
        }
        if expect_alphanumeric {
            return false;
        }
        match c {
            '.' => {}
            '_' => {
                if chars.peek() == Some(&'_') {
                    chars.next();
                }
            }
            '-' => {
                while chars.peek() == Some(&'-') {
                    chars.next();
                }
            }
            _ => return false,
        }
        expect_alphanumeric = true;
    }
    !expect_alphanumeric
}

/// The platform of a container node artifact. A `.sif` is a Linux image on
/// every host: on macOS, apptainer runs in a Linux VM.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ArtifactPlatform {
    #[serde(rename = "linux/amd64")]
    LinuxAmd64,
    #[serde(rename = "linux/arm64")]
    LinuxArm64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a platform peppy builds for: expected linux/amd64 or linux/arm64")]
pub struct ArtifactPlatformError(pub String);

impl ArtifactPlatform {
    /// Every platform, in the order peppy lists them.
    pub const ALL: [Self; 2] = [Self::LinuxAmd64, Self::LinuxArm64];

    /// Parses `linux/amd64` or `linux/arm64`.
    pub fn parse(raw: &str) -> Result<Self, ArtifactPlatformError> {
        let text = raw.trim();
        Self::ALL
            .into_iter()
            .find(|platform| platform.as_str() == text)
            .ok_or_else(|| ArtifactPlatformError(text.to_owned()))
    }

    /// The platform of the machine whose CPU architecture Rust names `arch`
    /// (`std::env::consts::ARCH`), or `None` for an architecture peppy does
    /// not build containers for.
    pub fn of_architecture(arch: &str) -> Option<Self> {
        match arch {
            "x86_64" => Some(Self::LinuxAmd64),
            "aarch64" => Some(Self::LinuxArm64),
            _ => None,
        }
    }

    /// `linux/amd64` or `linux/arm64`: the OCI spelling, which
    /// `--platforms` and the `bot.peppy.platform` annotation use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux/amd64",
            Self::LinuxArm64 => "linux/arm64",
        }
    }

    /// `linux-amd64` or `linux-arm64`: the spelling inside an artifact tag
    /// and an artifact file name, where `/` cannot appear.
    pub fn tag_suffix(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux-amd64",
            Self::LinuxArm64 => "linux-arm64",
        }
    }

    /// The CPU architecture as Rust names it (`x86_64`, `aarch64`): the
    /// spelling of the peppylib native extensions, the Cap'n Proto
    /// compilers, the apptainer builds and the Lima VM architectures.
    pub fn architecture(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "x86_64",
            Self::LinuxArm64 => "aarch64",
        }
    }
}

impl fmt::Display for ArtifactPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The peppy version an artifact is built for: the release tag of the peppy
/// that built it (`PEPPY_GIT_TAG`). A daemon reads only the artifacts of its
/// own version.
///
/// The text is what an OCI tag can hold: letters, digits, `.`, `_` and `-`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactVersion(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactVersionError {
    #[error("the peppy version is empty")]
    Empty,
    #[error(
        "the peppy version `{0}` holds a character that an OCI tag cannot hold (allowed: letters, \
         digits, `.`, `_` and `-`)"
    )]
    InvalidCharacter(String),
}

impl ArtifactVersion {
    pub fn parse(raw: &str) -> Result<Self, ArtifactVersionError> {
        if raw.is_empty() {
            return Err(ArtifactVersionError::Empty);
        }
        if !raw.chars().all(is_tag_character) {
            return Err(ArtifactVersionError::InvalidCharacter(raw.to_owned()));
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_tag_character(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The tag of one artifact in a publish repository:
/// `<node name>.<node tag>.<peppy version>.linux-<arch>`, for example
/// `waldo.v1.v0.36.0.linux-arm64`.
///
/// Node names and node tags hold no `.`, so the first `.`, the second `.`
/// and the last `.` split a tag without ambiguity, whatever `.` the version
/// holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactTag {
    node_name: String,
    node_tag: String,
    version: ArtifactVersion,
    platform: ArtifactPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactTagError {
    #[error(
        "the artifact tag `{tag}` of {node} is {length} characters; a registry accepts at most \
         {MAX_ARTIFACT_TAG_LENGTH}: give the node a shorter name or tag"
    )]
    TooLong {
        node: String,
        tag: String,
        length: usize,
    },
    #[error(
        "{node} has a name that starts with `-`, which cannot start an OCI tag: give the node a \
         name that starts with a letter, a digit or `_`"
    )]
    LeadingDash { node: String },
    #[error(
        "`{0}` is not a peppy artifact tag: expected <node name>.<node tag>.<peppy version>.linux-<arch>"
    )]
    NotAnArtifactTag(String),
}

impl ArtifactTag {
    /// The tag of the artifact of `node_name:node_tag` for `version` and
    /// `platform`. `node_name` and `node_tag` are the validated identity of
    /// a node, which holds only `A-Z a-z 0-9 _ -`.
    pub fn new(
        node_name: &str,
        node_tag: &str,
        version: &ArtifactVersion,
        platform: ArtifactPlatform,
    ) -> Result<Self, ArtifactTagError> {
        let node = format!("{node_name}:{node_tag}");
        if node_name.starts_with('-') {
            return Err(ArtifactTagError::LeadingDash { node });
        }
        let tag = Self {
            node_name: node_name.to_owned(),
            node_tag: node_tag.to_owned(),
            version: version.clone(),
            platform,
        };
        let text = tag.to_string();
        if text.len() > MAX_ARTIFACT_TAG_LENGTH {
            return Err(ArtifactTagError::TooLong {
                node,
                length: text.len(),
                tag: text,
            });
        }
        Ok(tag)
    }

    /// Parses a tag that a registry lists. Any tag that is not of the form
    /// `<node name>.<node tag>.<peppy version>.linux-<arch>` is refused, so
    /// a publish repository may hold other tags beside the artifacts.
    pub fn parse(raw: &str) -> Result<Self, ArtifactTagError> {
        let refuse = || ArtifactTagError::NotAnArtifactTag(raw.to_owned());
        let (node_name, rest) = raw.split_once('.').ok_or_else(refuse)?;
        let (node_tag, rest) = rest.split_once('.').ok_or_else(refuse)?;
        let (version, platform) = rest.rsplit_once('.').ok_or_else(refuse)?;
        let platform = ArtifactPlatform::ALL
            .into_iter()
            .find(|candidate| candidate.tag_suffix() == platform)
            .ok_or_else(refuse)?;
        config::repo_node_id::validate_repo_node_name(node_name, "name").map_err(|_| refuse())?;
        config::repo_node_id::validate_repo_node_tag(node_tag, "tag").map_err(|_| refuse())?;
        let version = ArtifactVersion::parse(version).map_err(|_| refuse())?;
        Self::new(node_name, node_tag, &version, platform).map_err(|_| refuse())
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }

    pub fn node_tag(&self) -> &str {
        &self.node_tag
    }

    pub fn version(&self) -> &ArtifactVersion {
        &self.version
    }

    pub fn platform(&self) -> ArtifactPlatform {
        self.platform
    }
}

impl fmt::Display for ArtifactTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.node_name,
            self.node_tag,
            self.version,
            self.platform.tag_suffix()
        )
    }
}

impl serde::Serialize for ArtifactTag {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ArtifactTag {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The digest of a blob or a manifest in an OCI registry: `sha256:<64
/// lowercase hex>`. Peppy writes only sha256 digests and reads no other.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OciDigest(String);

/// The algorithm prefix of every [`OciDigest`].
const SHA256_PREFIX: &str = "sha256:";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a sha256 digest: expected sha256:<64 hexadecimal characters>")]
pub struct OciDigestError(pub String);

impl OciDigest {
    /// Parses `sha256:<64 hex>`. Upper case hex reads as its lower case.
    pub fn parse(raw: &str) -> Result<Self, OciDigestError> {
        let refuse = || OciDigestError(raw.to_owned());
        let hex = raw.trim().strip_prefix(SHA256_PREFIX).ok_or_else(refuse)?;
        Self::from_sha256_hex(hex).map_err(|_| refuse())
    }

    /// The digest whose sha256 is `hex` (64 hexadecimal characters).
    pub fn from_sha256_hex(hex: &str) -> Result<Self, OciDigestError> {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(OciDigestError(hex.to_owned()));
        }
        Ok(Self(format!("{SHA256_PREFIX}{}", hex.to_ascii_lowercase())))
    }

    /// The 64 lowercase hexadecimal characters after `sha256:`.
    pub fn hex(&self) -> &str {
        &self.0[SHA256_PREFIX.len()..]
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OciDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for OciDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for OciDigest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn version(text: &str) -> ArtifactVersion {
        ArtifactVersion::parse(text).expect("valid version")
    }

    // -----------------------------------------------------------------------
    // OciRepository
    // -----------------------------------------------------------------------

    #[test]
    fn a_repository_url_splits_into_registry_and_path() {
        let repository = OciRepository::parse("oci://ghcr.io/acme/peppy-nodes").expect("parse");
        assert_eq!(repository.registry(), "ghcr.io");
        assert_eq!(repository.registry_host(), "ghcr.io");
        assert_eq!(repository.path(), "acme/peppy-nodes");
        assert_eq!(repository.to_string(), "oci://ghcr.io/acme/peppy-nodes");
        assert!(!repository.registry_is_loopback());
    }

    #[test]
    fn a_repository_url_keeps_a_port_and_lowercases_the_host() {
        let repository =
            OciRepository::parse(" oci://Registry.Example:5000/nodes/ ").expect("parse");
        assert_eq!(repository.registry(), "registry.example:5000");
        assert_eq!(repository.registry_host(), "registry.example");
        assert_eq!(repository.path(), "nodes");
        assert_eq!(repository.to_string(), "oci://registry.example:5000/nodes");
    }

    #[test]
    fn loopback_registries_are_recognized() {
        for url in [
            "oci://localhost:5000/nodes",
            "oci://localhost/nodes",
            "oci://127.0.0.1:5000/nodes",
            "oci://127.1.2.3/nodes",
            "oci://[::1]:5000/nodes",
        ] {
            let repository = OciRepository::parse(url).expect(url);
            assert!(repository.registry_is_loopback(), "{url}");
        }
        let ipv6 = OciRepository::parse("oci://[::1]:5000/nodes").expect("parse");
        assert_eq!(ipv6.registry(), "[::1]:5000");
        assert_eq!(ipv6.registry_host(), "[::1]");
        for url in ["oci://10.0.0.1/nodes", "oci://localhost.example/nodes"] {
            let repository = OciRepository::parse(url).expect(url);
            assert!(!repository.registry_is_loopback(), "{url}");
        }
    }

    #[test]
    fn path_components_follow_the_distribution_rules() {
        for path in [
            "a",
            "acme/peppy-nodes",
            "a.b/c_d/e__f/g---h",
            "0/9a",
            "deep/nested/repository/path",
        ] {
            let url = format!("oci://ghcr.io/{path}");
            assert!(OciRepository::parse(&url).is_ok(), "{url}");
        }
        for component in [
            "Acme", "a..b", "a___b", "-a", "a-", "_a", "a.", "a_.b", "a b", "a+b",
        ] {
            let url = format!("oci://ghcr.io/acme/{component}");
            assert_eq!(
                OciRepository::parse(&url),
                Err(OciRepositoryError::InvalidPathComponent {
                    url: url.clone(),
                    component: component.to_owned()
                }),
                "{url}"
            );
        }
        let url = "oci://ghcr.io/acme//nodes";
        assert_matches!(
            OciRepository::parse(url),
            Err(OciRepositoryError::InvalidPathComponent { .. })
        );
    }

    #[test]
    fn urls_that_name_no_oci_repository_are_refused() {
        assert_eq!(
            OciRepository::parse("https://ghcr.io/acme/nodes"),
            Err(OciRepositoryError::NotOciScheme(
                "https://ghcr.io/acme/nodes".to_owned()
            ))
        );
        assert_eq!(
            OciRepository::parse("ghcr.io/acme/nodes"),
            Err(OciRepositoryError::NotAUrl("ghcr.io/acme/nodes".to_owned()))
        );
        assert_eq!(
            OciRepository::parse("oci://ghcr.io"),
            Err(OciRepositoryError::NoPath("oci://ghcr.io".to_owned()))
        );
        assert_eq!(
            OciRepository::parse("oci://ghcr.io/"),
            Err(OciRepositoryError::NoPath("oci://ghcr.io/".to_owned()))
        );
        assert_eq!(
            OciRepository::parse("oci:///acme/nodes"),
            Err(OciRepositoryError::NoHost("oci:///acme/nodes".to_owned()))
        );
        assert_eq!(
            OciRepository::parse("oci://user:secret@ghcr.io/acme/nodes"),
            Err(OciRepositoryError::Credentials)
        );
        assert!(
            !OciRepositoryError::Credentials
                .to_string()
                .contains("secret")
        );
        assert_eq!(
            OciRepository::parse("oci://ghcr.io/acme/nodes?x=1"),
            Err(OciRepositoryError::QueryOrFragment(
                "oci://ghcr.io/acme/nodes?x=1".to_owned()
            ))
        );
        assert_eq!(
            OciRepository::parse("oci://ghcr.io/acme/nodes#x"),
            Err(OciRepositoryError::QueryOrFragment(
                "oci://ghcr.io/acme/nodes#x".to_owned()
            ))
        );
    }

    #[test]
    fn a_repository_name_above_the_registry_limit_is_refused() {
        let path = "a".repeat(MAX_REPOSITORY_NAME_LENGTH - "ghcr.io/".len());
        OciRepository::parse(&format!("oci://ghcr.io/{path}")).unwrap();
        let url = format!("oci://ghcr.io/{path}a");
        assert_eq!(
            OciRepository::parse(&url),
            Err(OciRepositoryError::TooLong {
                url: url.clone(),
                length: MAX_REPOSITORY_NAME_LENGTH + 1
            })
        );
    }

    #[test]
    fn a_repository_round_trips_through_serde_as_its_url() {
        let repository = OciRepository::parse("oci://ghcr.io/acme/nodes").expect("parse");
        let json = serde_json::to_string(&repository).expect("serialize");
        assert_eq!(json, "\"oci://ghcr.io/acme/nodes\"");
        let back: OciRepository = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, repository);
        serde_json::from_str::<OciRepository>("\"https://ghcr.io/x\"").unwrap_err();
    }

    // -----------------------------------------------------------------------
    // ArtifactPlatform
    // -----------------------------------------------------------------------

    #[test]
    fn platforms_parse_from_their_oci_spelling_only() {
        assert_eq!(
            ArtifactPlatform::parse("linux/amd64"),
            Ok(ArtifactPlatform::LinuxAmd64)
        );
        assert_eq!(
            ArtifactPlatform::parse(" linux/arm64 "),
            Ok(ArtifactPlatform::LinuxArm64)
        );
        for raw in [
            "linux/x86_64",
            "linux-amd64",
            "darwin/arm64",
            "",
            "linux/arm",
        ] {
            assert_eq!(
                ArtifactPlatform::parse(raw),
                Err(ArtifactPlatformError(raw.trim().to_owned())),
                "{raw}"
            );
        }
    }

    #[test]
    fn platforms_name_their_architecture_in_each_spelling() {
        assert_eq!(
            ArtifactPlatform::of_architecture("x86_64"),
            Some(ArtifactPlatform::LinuxAmd64)
        );
        assert_eq!(
            ArtifactPlatform::of_architecture("aarch64"),
            Some(ArtifactPlatform::LinuxArm64)
        );
        assert_eq!(ArtifactPlatform::of_architecture("riscv64"), None);
        assert_eq!(ArtifactPlatform::LinuxArm64.tag_suffix(), "linux-arm64");
        assert_eq!(ArtifactPlatform::LinuxAmd64.architecture(), "x86_64");
        assert_eq!(
            serde_json::to_string(&ArtifactPlatform::LinuxArm64).expect("serialize"),
            "\"linux/arm64\""
        );
    }

    // -----------------------------------------------------------------------
    // ArtifactTag
    // -----------------------------------------------------------------------

    #[test]
    fn a_tag_is_written_and_parsed_back() {
        let tag = ArtifactTag::new(
            "waldo",
            "v1",
            &version("v0.36.0"),
            ArtifactPlatform::LinuxArm64,
        )
        .expect("tag");
        assert_eq!(tag.to_string(), "waldo.v1.v0.36.0.linux-arm64");
        assert_eq!(ArtifactTag::parse("waldo.v1.v0.36.0.linux-arm64"), Ok(tag));
    }

    #[test]
    fn names_and_tags_with_dashes_and_underscores_split_without_ambiguity() {
        for (name, node_tag, peppy_version) in [
            ("vlm_router-brain", "v2_beta-1", "v0.36.0"),
            ("_hidden", "A-B", "dev-5b706c5a0f1e"),
            ("9lives", "x", "v1.2.3-rc.1"),
            ("a", "b", "c"),
        ] {
            for platform in ArtifactPlatform::ALL {
                let tag = ArtifactTag::new(name, node_tag, &version(peppy_version), platform)
                    .expect("tag");
                let parsed = ArtifactTag::parse(&tag.to_string()).expect("parse");
                assert_eq!(parsed.node_name(), name);
                assert_eq!(parsed.node_tag(), node_tag);
                assert_eq!(parsed.version().as_str(), peppy_version);
                assert_eq!(parsed.platform(), platform);
            }
        }
    }

    #[test]
    fn tags_that_are_not_artifacts_are_refused() {
        for raw in [
            "latest",
            "waldo.v1",
            "waldo.v1.linux-arm64",
            "waldo.v1.v0.36.0.linux-riscv64",
            "waldo.v1.v0.36.0.darwin-arm64",
            ".v1.v0.36.0.linux-arm64",
            "waldo..v0.36.0.linux-arm64",
            "-waldo.v1.v0.36.0.linux-arm64",
            "wal do.v1.v0.36.0.linux-arm64",
        ] {
            assert_eq!(
                ArtifactTag::parse(raw),
                Err(ArtifactTagError::NotAnArtifactTag(raw.to_owned())),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_tag_above_the_registry_limit_is_refused_and_names_the_node() {
        let platform = ArtifactPlatform::LinuxAmd64;
        let peppy_version = version("v0.36.0");
        let fixed = ".v1.v0.36.0.linux-amd64".len();
        let longest = "n".repeat(MAX_ARTIFACT_TAG_LENGTH - fixed);
        let tag = ArtifactTag::new(&longest, "v1", &peppy_version, platform).expect("at limit");
        assert_eq!(tag.to_string().len(), MAX_ARTIFACT_TAG_LENGTH);

        let too_long = format!("{longest}n");
        let error = ArtifactTag::new(&too_long, "v1", &peppy_version, platform)
            .expect_err("above the limit");
        assert_eq!(
            error,
            ArtifactTagError::TooLong {
                node: format!("{too_long}:v1"),
                tag: format!("{too_long}.v1.v0.36.0.linux-amd64"),
                length: MAX_ARTIFACT_TAG_LENGTH + 1,
            }
        );
        assert!(error.to_string().contains(&format!("{too_long}:v1")));
    }

    #[test]
    fn a_node_name_that_starts_with_a_dash_cannot_be_a_tag() {
        assert_eq!(
            ArtifactTag::new(
                "-x",
                "v1",
                &version("v0.36.0"),
                ArtifactPlatform::LinuxAmd64
            ),
            Err(ArtifactTagError::LeadingDash {
                node: "-x:v1".to_owned()
            })
        );
    }

    #[test]
    fn versions_hold_only_tag_characters() {
        ArtifactVersion::parse("v0.36.0").unwrap();
        ArtifactVersion::parse("dev-5b706c5a0f1e").unwrap();
        assert_eq!(ArtifactVersion::parse(""), Err(ArtifactVersionError::Empty));
        for raw in ["v1+build", "v1/2", "v 1"] {
            assert_eq!(
                ArtifactVersion::parse(raw),
                Err(ArtifactVersionError::InvalidCharacter(raw.to_owned())),
                "{raw}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // OciDigest
    // -----------------------------------------------------------------------

    #[test]
    fn a_digest_is_sha256_and_64_hex_characters() {
        let hex = "A".repeat(64);
        let digest = OciDigest::parse(&format!("sha256:{hex}")).expect("parse");
        assert_eq!(digest.hex(), "a".repeat(64));
        assert_eq!(digest.as_str(), format!("sha256:{}", "a".repeat(64)));
        assert_eq!(OciDigest::from_sha256_hex(&hex), Ok(digest.clone()));
        let json = serde_json::to_string(&digest).expect("serialize");
        assert_eq!(
            serde_json::from_str::<OciDigest>(&json).expect("deserialize"),
            digest
        );
        for raw in [
            String::new(),
            "a".repeat(64),
            format!("sha512:{}", "a".repeat(64)),
            format!("sha256:{}", "a".repeat(63)),
            format!("sha256:{}", "g".repeat(64)),
        ] {
            assert_eq!(
                OciDigest::parse(&raw),
                Err(OciDigestError(raw.clone())),
                "{raw}"
            );
        }
    }
}
