use std::path::PathBuf;

/// Why a request to an OCI registry failed.
///
/// Each message names the registry, so that a caller only adds what it was
/// doing (the repository, the tag, the digest) in front of it.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No answer: no connection, a TLS failure, a timeout.
    #[error("the registry {registry} is not reachable: {cause}")]
    Unreachable { registry: String, cause: String },
    /// The registry asks for a credential, and the auth file names none for
    /// it.
    #[error(
        "the registry {registry} asks for a credential, and the registry auth file names none \
         for it: log in with `docker login {registry}` or `apptainer registry login`"
    )]
    NoCredential { registry: String },
    /// The registry refused the credential that the auth file names.
    #[error("the registry {registry} refused the credential of the registry auth file: {detail}")]
    Unauthorized { registry: String, detail: String },
    /// The registry answered with a status that the request does not
    /// expect.
    #[error("the registry {registry} answered {status} to {request}: {detail}")]
    Refused {
        registry: String,
        request: String,
        status: u16,
        detail: String,
    },
    /// The registry does not have the manifest or the blob.
    #[error("the registry {registry} has no {what}")]
    NotFound { registry: String, what: String },
    /// The bytes the registry sent do not have the digest they were asked
    /// by.
    #[error("{what} has the digest {actual}, not {expected}")]
    DigestMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    /// The registry sent more bytes than the descriptor states, or fewer.
    #[error("{what} is {actual} bytes, not {expected}")]
    SizeMismatch {
        what: String,
        expected: u64,
        actual: u64,
    },
    /// An answer that does not follow the OCI distribution specification.
    #[error("the registry {registry} answered {request} with {detail}")]
    Protocol {
        registry: String,
        request: String,
        detail: String,
    },
    /// A credential helper that the auth file names failed.
    #[error("the credential helper docker-credential-{helper} failed: {detail}")]
    CredentialHelper { helper: String, detail: String },
    #[error(transparent)]
    AuthFile(#[from] containers::Error),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RegistryError {
    /// A file of this machine that a pull or a push could not read or write.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, RegistryError>;
