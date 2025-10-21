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
    #[error(
        "service '{service_name}' in namespace '{namespace}' on node '{service_node}' is unreachable"
    )]
    ServiceUnreachable {
        service_node: String,
        namespace: String,
        service_name: String,
    },
    // #[error(
    //     "service '{service_name}' in namespace '{namespace}' on node '{service_node}' has timed out"
    // )]
    // ServiceTimeout {
    //     service_node: String,
    //     namespace: String,
    //     service_name: String,
    // },
}
