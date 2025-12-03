use std::env::VarError;
use std::fmt;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // -- config
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    // -- serde
    #[error(transparent)]
    SerdeJson5(#[from] serde_json5::Error),

    // -- pmi-internal
    #[error(transparent)]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),

    #[error("invalid service request '{identifier}': {reason}")]
    InvalidServiceRequest { identifier: String, reason: String },

    #[error("service request stream closed unexpectedly")]
    ServiceRequestStreamClosed,

    // -- topics/services/actions errors
    #[error(
        "service '{service_name}'{instance_suffix} is unreachable",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceUnreachable {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "service '{service_name}'{instance_suffix} has timed out",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceTimeout {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} has timed out waiting for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultTimeout {
        instance_id: Option<String>,
        action_name: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} is unreachable for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultUnreachable {
        instance_id: Option<String>,
        action_name: String,
    },

    // -- system
    #[error("failed to read `{var}` from the environment")]
    MissingInstanceIdEnvVar {
        var: &'static str,
        #[source]
        source: VarError,
    },

    #[error("failed to read launch config at `{path}`")]
    LaunchConfigRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse launch config at `{path}`")]
    LaunchConfigParse {
        path: String,
        #[source]
        source: serde_json5::Error,
    },

    #[error("peppy config md5 mismatch for `{path}` (expected `{expected}`, got `{actual}`)")]
    PeppyConfigMd5Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
}

struct InstanceSuffix<'a>(Option<&'a str>);

impl fmt::Display for InstanceSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(instance_id) = self.0 {
            write!(f, " for instance '{instance_id}'")
        } else {
            Ok(())
        }
    }
}
