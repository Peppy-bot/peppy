//! This machine's router peer identity: a freshly minted key pair and a
//! certificate signing request naming the peer. The private key never leaves
//! the machine; only the request travels to the platform, which signs it into
//! the client certificate the daemon presents to the project's cloud router.

use secrecy::SecretString;

use crate::error::{Error, Result};
use crate::storage::secret;

/// Longest common name the platform accepts, in bytes of UTF-8.
const MAX_NAME_BYTES: usize = 64;

/// The name this machine enrolls under: the certificate's single common name
/// and the peer's display name on the platform.
///
/// Constructed only through [`PeerName::parse`], which applies the platform's
/// own rails so a request it would refuse (`422 malformed-csr`) never leaves the
/// machine: 1 to 64 bytes of UTF-8, no leading or trailing whitespace, and no
/// control, bidirectional-formatting or zero-width characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerName(String);

impl PeerName {
    pub fn parse(raw: &str) -> Result<Self> {
        let invalid = |reason: &str| Error::Auth(format!("invalid peer name {raw:?}: {reason}"));
        if raw.is_empty() {
            return Err(invalid("must not be empty"));
        }
        if raw.len() > MAX_NAME_BYTES {
            return Err(invalid(&format!("must be at most {MAX_NAME_BYTES} bytes")));
        }
        if raw.trim() != raw {
            return Err(invalid("must not start or end with whitespace"));
        }
        if raw.chars().any(is_invisible) {
            return Err(invalid(
                "must not contain control, bidirectional-formatting or zero-width characters",
            ));
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Characters that render as nothing or reorder what follows, so a name
/// carrying one would look like a different name than the platform stores.
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{FEFF}'
        )
}

/// A minted key pair and the signing request for it.
pub struct PeerIdentity {
    /// The PKCS#8 private key, PEM encoded. Held as a secret so it never
    /// surfaces in `Debug` or log output.
    pub private_key_pem: SecretString,
    /// The PKCS#10 request, PEM encoded: one common name, no extensions.
    pub csr_pem: String,
}

/// Mints a P-256 key pair and a signing request whose subject is exactly one
/// common name, `name`, and which asks for no extensions. Both are what the
/// platform's enrollment endpoint requires.
pub fn generate_peer_identity(name: &PeerName) -> Result<PeerIdentity> {
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| Error::Auth(format!("generating the peer key pair failed: {e}")))?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|e| Error::Auth(format!("building the certificate request failed: {e}")))?;
    let mut subject = rcgen::DistinguishedName::new();
    subject.push(rcgen::DnType::CommonName, name.as_str());
    params.distinguished_name = subject;
    let csr_pem = params
        .serialize_request(&key_pair)
        .and_then(|request| request.pem())
        .map_err(|e| Error::Auth(format!("signing the certificate request failed: {e}")))?;
    Ok(PeerIdentity {
        private_key_pem: secret(key_pair.serialize_pem()),
        csr_pem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;
    use secrecy::ExposeSecret;

    #[test]
    fn peer_names_follow_the_platform_rails() {
        for accepted in [
            "robot-7",
            "Lab bench 1",
            "日本語",
            &"n".repeat(MAX_NAME_BYTES),
        ] {
            PeerName::parse(accepted).unwrap_or_else(|e| panic!("{accepted:?}: {e}"));
        }
        for rejected in [
            "",
            " robot",
            "robot ",
            "tab\there",
            "zero\u{200B}width",
            "bidi\u{202E}flip",
            &"n".repeat(MAX_NAME_BYTES + 1),
        ] {
            assert!(
                PeerName::parse(rejected).is_err(),
                "{rejected:?} must be rejected"
            );
        }
    }

    /// What the platform receives: one common name, no extension request, a
    /// P-256 key, and a request the minted key actually signed.
    #[test]
    fn the_request_carries_one_common_name_and_no_extensions() {
        let name = PeerName::parse("robot-7").unwrap();
        let identity = generate_peer_identity(&name).expect("mint");

        let parsed = rcgen::CertificateSigningRequestParams::from_pem(&identity.csr_pem)
            .expect("the request parses and its signature verifies");
        let subject: Vec<_> = parsed.params.distinguished_name.iter().collect();
        assert_eq!(subject.len(), 1, "exactly one subject attribute");
        assert_eq!(
            subject[0],
            (
                &rcgen::DnType::CommonName,
                &rcgen::DnValue::Utf8String("robot-7".to_string())
            )
        );
        assert!(parsed.params.subject_alt_names.is_empty());
        assert!(parsed.params.key_usages.is_empty());
        assert!(parsed.params.extended_key_usages.is_empty());
        assert!(parsed.params.custom_extensions.is_empty());
        assert_eq!(
            parsed.public_key.algorithm(),
            &rcgen::PKCS_ECDSA_P256_SHA256
        );

        let key = rcgen::KeyPair::from_pem(identity.private_key_pem.expose_secret())
            .expect("the key is PKCS#8 PEM");
        assert_eq!(
            key.der_bytes(),
            parsed.public_key.der_bytes(),
            "the request carries the minted key's public half"
        );
        assert!(
            identity
                .private_key_pem
                .expose_secret()
                .starts_with("-----BEGIN PRIVATE KEY-----")
        );
    }
}
