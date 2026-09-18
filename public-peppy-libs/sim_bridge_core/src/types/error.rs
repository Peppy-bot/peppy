use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum BridgeError {
    ConfigNotFound {
        path: PathBuf,
        source: std::io::Error,
    },
    ConfigParse(String),
    InvalidPreset(String),
    UnknownTopicType(String),
    JointResolution(String),
    Io(std::io::Error),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigNotFound { path, source } => {
                write!(f, "config not found at '{}': {source}", path.display())
            }
            Self::ConfigParse(msg) => write!(f, "config parse error: {msg}"),
            Self::InvalidPreset(name) => write!(f, "invalid preset name '{name}'"),
            Self::UnknownTopicType(t) => write!(f, "unknown topic type '{t}'"),
            Self::JointResolution(msg) => write!(f, "joint resolution error: {msg}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for BridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ConfigNotFound { source, .. } => Some(source),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BridgeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, BridgeError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_preset() {
        let e = BridgeError::InvalidPreset("bad name!".into());
        assert_eq!(e.to_string(), "invalid preset name 'bad name!'");
    }

    #[test]
    fn display_unknown_topic_type() {
        let e = BridgeError::UnknownTopicType("foo".into());
        assert_eq!(e.to_string(), "unknown topic type 'foo'");
    }
}
