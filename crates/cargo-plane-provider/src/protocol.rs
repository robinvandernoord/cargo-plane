//! What cargo-plane and a plugin say to each other. You only need this to
//! write a plugin without [`run`](crate::run), e.g. in another language.
//!
//! cargo-plane listens on a Unix socket, starts the plugin with the socket's
//! path in [`SOCKET_ENV`] and waits for it to connect. Both sides then send
//! one JSON object per line:
//!
//! 1. The plugin sends [`Message::Hello`]; cargo-plane hangs up if the
//!    protocol version differs.
//! 2. cargo-plane sends a [`Request`]. While handling it, the plugin may send
//!    callbacks (the other [`Message`] variants), each answered by a
//!    [`Reply`], and finally [`Message::Done`] or [`Message::Failed`].
//! 3. Repeat from 2. When cargo-plane closes the socket, the plugin exits.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Setup;

/// Bumped on every incompatible change.
pub const VERSION: u32 = 1;

/// Environment variable with the socket path cargo-plane listens on.
pub const SOCKET_ENV: &str = "CARGO_PLANE_PROVIDER_SOCKET";

/// cargo-plane asking the plugin to do something.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Answer with a [`Started`](crate::Started).
    Up { config: Value },
    /// Answer with `null`.
    Down { config: Value },
    /// Answer with a string.
    Status { config: Value },
    /// Answer with a bool: whether it was purged.
    Purge { config: Value },
    /// Answer with `null`.
    Setup { setup: Setup },
}

/// The plugin talking to cargo-plane.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello {
        protocol: u32,
        /// Version of this crate the plugin was built with, for error messages.
        sdk: String,
    },
    /// The request succeeded.
    Done {
        result: Value,
    },
    /// The request failed.
    Failed {
        error: String,
    },

    // Callbacks; see `Host` for what they do. Each gets a `Reply`.
    Status {
        verb: String,
        message: String,
    },
    Warn {
        message: String,
    },
    /// Reply: bool.
    Interactive,
    /// Reply: string.
    Ask {
        question: String,
        default: Option<String>,
    },
    /// Reply: bool.
    Confirm {
        question: String,
        default: bool,
    },
    SaveConfig {
        config: Value,
    },
    /// Reply: a [`RemoteOutput`](crate::RemoteOutput).
    RunRemote {
        command: String,
        capture: bool,
    },
}

/// cargo-plane answering a callback.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ok { value: Value },
    Err { error: String },
}

/// Write one message as a line of JSON.
pub fn send<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Read one message; `None` when the other side hung up.
pub fn recv<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .with_context(|| format!("unexpected message: {}", line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_lines() {
        let mut buf = Vec::new();
        send(
            &mut buf,
            &Message::Ask {
                question: "Region".into(),
                default: Some("ams".into()),
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap(),
            "{\"type\":\"ask\",\"question\":\"Region\",\"default\":\"ams\"}\n"
        );
        let back: Message = recv(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(back, Message::Ask { question, .. } if question == "Region"));
        assert!(recv::<Message>(&mut &b""[..]).unwrap().is_none());
    }
}
