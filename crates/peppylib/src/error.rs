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

    // -- topics/services/actions errors
    #[error("service '{service_name}' for instance '{instance_id}' is unreachable")]
    ServiceUnreachable {
        instance_id: String,
        service_name: String,
    },
    #[error("service '{service_name}' for instance '{instance_id}' has timed out")]
    ServiceTimeout {
        instance_id: String,
        service_name: String,
    },
    #[error("action '{action_name}' for instance '{instance_id}' has timed out waiting for result")]
    ActionResultTimeout {
        instance_id: String,
        action_name: String,
    },
    #[error("action '{action_name}' for instance '{instance_id}' is unreachable for result")]
    ActionResultUnreachable {
        instance_id: String,
        action_name: String,
    },
}
