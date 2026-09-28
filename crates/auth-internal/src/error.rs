use std::fmt;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- filesystem (credential and enrollment reads/writes)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- transport/HTTP failures (unreachable backend, unexpected status)
    #[error("{0}")]
    Http(String),

    // -- OAuth / identity / enrollment failures with a user-actionable message
    #[error("{0}")]
    Auth(String),

    // -- a request the platform refused, with the problem document it sent
    #[error("{0}")]
    Problem(Problem),

    // -- no usable session
    #[error("Not authenticated. Run `peppy platform login`.")]
    NotAuthenticated,
}

/// The problem types of the platform contract that a command acts on. Every
/// other type is kept as [`ProblemKind::Other`], so a refusal this CLI does not
/// know still prints its title and detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemKind {
    /// The router holds as many peers as the plan admits. A removed peer keeps
    /// its slot until the router restarts.
    PeerLimitReached,
    /// The platform could not sign the certificate in time.
    ProvisionerUnavailable,
    /// The person stopped the router.
    RouterStopped,
    /// The certificate signing request is not one the router can use.
    MalformedCsr,
    /// A type this CLI has no special handling for, as the platform sent it.
    Other(String),
}

impl ProblemKind {
    /// Parses the `type` member of a problem document. The platform names a
    /// type by a URL whose last path segment is the name.
    pub fn parse(problem_type: &str) -> Self {
        match problem_type.rsplit('/').next().unwrap_or_default() {
            "peer-limit-reached" => Self::PeerLimitReached,
            "provisioner-unavailable" => Self::ProvisionerUnavailable,
            "router-stopped" => Self::RouterStopped,
            "malformed-csr" => Self::MalformedCsr,
            _ => Self::Other(problem_type.to_string()),
        }
    }
}

/// A refusal from the platform (RFC 9457). Commands branch on `kind` and on
/// `status`, never on the text of `title` or `detail`, which is for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub kind: ProblemKind,
    pub status: u16,
    pub title: String,
    pub detail: Option<String>,
    /// The delay the platform asked for in `Retry-After`, in seconds.
    pub retry_after_secs: Option<u64>,
    /// How many slots of the router a restart frees, when the platform says
    /// so on a [`ProblemKind::PeerLimitReached`] refusal. `None` when the
    /// platform does not say; the caller then reads the peers to find out.
    pub pending_removals: Option<u32>,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.detail.as_deref().filter(|detail| !detail.is_empty()) {
            Some(detail) => write!(f, "{}: {detail}", self.title),
            None => f.write_str(&self.title),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_is_the_last_segment_of_the_type() {
        for (problem_type, kind) in [
            (
                "https://peppy.bot/problems/peer-limit-reached",
                ProblemKind::PeerLimitReached,
            ),
            (
                "https://peppy.bot/problems/provisioner-unavailable",
                ProblemKind::ProvisionerUnavailable,
            ),
            (
                "https://peppy.bot/problems/router-stopped",
                ProblemKind::RouterStopped,
            ),
            (
                "https://peppy.bot/problems/malformed-csr",
                ProblemKind::MalformedCsr,
            ),
            (
                "https://peppy.bot/problems/size-not-entitled",
                ProblemKind::Other("https://peppy.bot/problems/size-not-entitled".into()),
            ),
            ("about:blank", ProblemKind::Other("about:blank".into())),
            ("", ProblemKind::Other(String::new())),
        ] {
            assert_eq!(ProblemKind::parse(problem_type), kind, "{problem_type}");
        }
    }

    #[test]
    fn a_problem_prints_its_title_and_its_detail() {
        let mut problem = Problem {
            kind: ProblemKind::PeerLimitReached,
            status: 422,
            title: "Peer limit reached".into(),
            detail: Some("this router admits 5 peers".into()),
            retry_after_secs: None,
            pending_removals: None,
        };
        assert_eq!(
            problem.to_string(),
            "Peer limit reached: this router admits 5 peers"
        );
        problem.detail = None;
        assert_eq!(problem.to_string(), "Peer limit reached");
    }
}
