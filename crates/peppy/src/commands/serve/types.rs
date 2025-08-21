use std::fmt;
use std::str::FromStr;

use super::error::ServeCommandError;

pub struct MessagingConfiguration {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
}

impl MessagingConfiguration {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            engine: Engine::Zenoh, // Default engine
            host: host.to_string(),
            port,
        }
    }

    pub fn with_engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Engine {
    Zenoh,
    Mock,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Engine::Zenoh => write!(f, "zenoh"),
            Engine::Mock => write!(f, "mock"),
        }
    }
}

impl FromStr for Engine {
    type Err = ServeCommandError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "zenoh" => Ok(Engine::Zenoh),
            "mock" => Ok(Engine::Mock),
            _ => Err(ServeCommandError::UnsupportedEngine),
        }
    }
}
