pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- filesystem (credential cache reads/writes)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- transport/HTTP failures (unreachable backend, unexpected status)
    #[error("{0}")]
    Http(String),

    // -- OAuth / identity failures with a user-actionable message
    #[error("{0}")]
    Auth(String),

    // -- typed certificate/discovery binding drift; the production resolver
    // uses this to trigger immediate same-owner re-enrollment rather than
    // string-matching an error or waiting until renew_after.
    #[error(
        "federation workspace {discovered} does not match the core-node certificate workspace {certificate}; re-enrollment is required"
    )]
    WorkspaceMismatch {
        discovered: config::namespace::Namespace,
        certificate: config::namespace::Namespace,
    },

    // The discovery endpoint returns this stable conflict instead of leaking a
    // router config for the certificate's former workspace. The router layer
    // compares it with the locally validated certificate binding before
    // triggering a fresh-key enrollment.
    #[error(
        "the platform reports that this core node now belongs to workspace {current}; certificate re-enrollment is required"
    )]
    DiscoveryWorkspaceMismatch {
        current: config::namespace::Namespace,
    },

    #[error(
        "core-node name `{0}` is already reserved by another account; choose a unique core_node_name and restart the daemon"
    )]
    CoreNodeNameTaken(String),

    #[error(
        "core-node enrollment `{0}` is revoked; a fresh key is required, so retry `peppy platform login`"
    )]
    CoreNodeRevoked(String),

    #[error(
        "the generated key for core node `{0}` was already used by another certificate; it was discarded, so retry enrollment to generate a fresh key"
    )]
    CoreNodeKeyAlreadyUsed(String),

    /// The command was accepted for one OAuth login, but another fresh login
    /// replaced it before the operation could safely commit its result.
    #[error("the platform session changed while this operation was in flight")]
    StaleSessionRevision,

    #[error(
        "the daemon has no configured PEPPY_API_KEY and no OAuth session revision was supplied; retry `peppy platform login` or configure PEPPY_API_KEY"
    )]
    PatNotConfigured,

    #[error(
        "the daemon service PEPPY_API_KEY is active, so it cannot enroll an OAuth session; remove the service key and restart the daemon before retrying OAuth login"
    )]
    PatActive,

    #[error(
        "the daemon service PEPPY_API_KEY belongs to a different platform principal than this shell's key"
    )]
    PatPrincipalMismatch,

    // -- no usable credential and not on an interactive terminal
    #[error("Not authenticated. Run `peppy platform login` or set PEPPY_API_KEY.")]
    NotAuthenticated,
}
