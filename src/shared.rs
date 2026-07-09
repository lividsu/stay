use serde::{Deserialize, Serialize};
use std::fmt;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub type StayResult<T> = Result<T, StayError>;

#[derive(Debug)]
pub struct StayError {
    message: String,
}

impl StayError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StayError {}

impl From<std::io::Error> for StayError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<serde_json::Error> for StayError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Running,
    Exited,
    Stopped,
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionState::Running => write!(f, "running"),
            SessionState::Exited => write!(f, "exited"),
            SessionState::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionRecord {
    pub name: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub state: SessionState,
    pub pid: Option<i32>,
    pub created_at: String,
    pub last_attached_at: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Attach {
        name: String,
        cwd: String,
        command: Option<Vec<String>>,
        restart: bool,
        rows: u16,
        cols: u16,
    },
    Kill {
        name: String,
    },
    List,
    Remove {
        name: String,
    },
    DaemonInfo,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    AttachReady {
        message: String,
    },
    Error {
        message: String,
    },
    HistoryReady {
        state: SessionState,
        exit_code: Option<i32>,
        command: Vec<String>,
    },
    NeedsRestart {
        name: String,
        state: SessionState,
        exit_code: Option<i32>,
        command: Vec<String>,
    },
    Ok {
        message: String,
    },
    Sessions {
        sessions: Vec<SessionRecord>,
    },
    DaemonInfo {
        version: String,
    },
}

pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "Invalid session name: {name}\n\nUse letters, numbers, \"-\", \"_\" or \".\"."
        ));
    }

    Ok(())
}

pub fn shell_command() -> Vec<String> {
    vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())]
}

pub fn display_command(command: &[String]) -> String {
    if command.is_empty() {
        return "".to_string();
    }

    command
        .iter()
        .map(|part| {
            if part.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'/' | b'.' | b'-' | b'_' | b':' | b'=' | b'+' | b',' | b'@'
                    )
            }) {
                part.clone()
            } else {
                format!("'{}'", part.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_simple_session_names() {
        for name in ["api", "web_1", "dev-server", "project.test"] {
            assert!(validate_session_name(name).is_ok());
        }
    }

    #[test]
    fn rejects_invalid_session_names() {
        for name in ["", "my api", "api/log", "..", "a..b"] {
            assert!(validate_session_name(name).is_err());
        }

        assert!(validate_session_name(&"a".repeat(65)).is_err());
    }
}
