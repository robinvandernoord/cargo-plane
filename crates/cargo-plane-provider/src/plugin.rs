//! The plugin's side of the protocol.

use std::cell::RefCell;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::protocol::{self, Message, Reply, Request};
use crate::{AnyProvider, Host, Provider, RemoteOutput};

/// Run `provider` as a cargo-plane plugin: connect to cargo-plane and handle
/// its requests until it hangs up. Call it from `main`:
///
/// ```no_run
/// # struct MyProvider;
/// # impl cargo_plane_provider::Provider for MyProvider {
/// #     type Config = ();
/// #     fn up(&self, _: &(), _: &dyn cargo_plane_provider::Host) -> anyhow::Result<cargo_plane_provider::Endpoint> { todo!() }
/// #     fn down(&self, _: &(), _: &dyn cargo_plane_provider::Host) -> anyhow::Result<()> { todo!() }
/// #     fn status(&self, _: &(), _: &dyn cargo_plane_provider::Host) -> anyhow::Result<String> { todo!() }
/// # }
/// fn main() -> std::process::ExitCode {
///     cargo_plane_provider::run(MyProvider)
/// }
/// ```
pub fn run<P: Provider>(provider: P) -> ExitCode {
    match connect().and_then(|stream| serve_on(stream, &provider)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn connect() -> Result<UnixStream> {
    let Some(path) = std::env::var_os(protocol::SOCKET_ENV) else {
        let name = std::env::args()
            .next()
            .and_then(|arg0| {
                let file = std::path::Path::new(&arg0)
                    .file_name()?
                    .to_str()?
                    .to_owned();
                file.strip_prefix("cargo-plane-provider-")
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "<name>".into());
        bail!(
            "this is a cargo-plane provider plugin, not meant to be run directly; \
             cargo-plane runs it for remotes with `provider = \"{name}\"`"
        );
    };
    UnixStream::connect(&path).with_context(|| format!("connecting to cargo-plane at {path:?}"))
}

/// Handle requests on an already connected socket until cargo-plane hangs up.
#[doc(hidden)]
pub fn serve_on(stream: UnixStream, provider: &dyn AnyProvider) -> Result<()> {
    let channel = RefCell::new(Channel {
        reader: BufReader::new(stream.try_clone()?),
        writer: stream,
    });
    channel.borrow_mut().send(&Message::Hello {
        protocol: protocol::VERSION,
        sdk: env!("CARGO_PKG_VERSION").into(),
    })?;

    loop {
        let request = channel.borrow_mut().recv::<Request>()?;
        let Some(request) = request else {
            return Ok(());
        };
        let host = SocketHost { channel: &channel };
        let result = match request {
            Request::Up { config } => provider
                .up(&config, &host)
                .and_then(|started| Ok(serde_json::to_value(started)?)),
            Request::Down { config } => provider.down(&config, &host).map(|()| Value::Null),
            Request::Status { config } => provider.status(&config, &host).map(Value::String),
            Request::Purge { config } => provider.purge(&config, &host).map(Value::Bool),
            Request::Setup { setup } => provider.setup(&setup, &host).map(|()| Value::Null),
        };
        let message = match result {
            Ok(result) => Message::Done { result },
            Err(err) => Message::Failed {
                error: format!("{err:#}"),
            },
        };
        channel.borrow_mut().send(&message)?;
    }
}

struct Channel {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Channel {
    fn send(&mut self, message: &Message) -> Result<()> {
        protocol::send(&mut self.writer, message).context("cargo-plane hung up")
    }

    fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        protocol::recv(&mut self.reader)
    }
}

/// A [`Host`] that forwards every call to cargo-plane.
struct SocketHost<'a> {
    channel: &'a RefCell<Channel>,
}

impl SocketHost<'_> {
    fn call<T: DeserializeOwned>(&self, message: Message) -> Result<T> {
        let mut channel = self.channel.borrow_mut();
        channel.send(&message)?;
        match channel.recv::<Reply>()? {
            Some(Reply::Ok { value }) => Ok(serde_json::from_value(value)?),
            Some(Reply::Err { error }) => Err(anyhow!(error)),
            None => bail!("cargo-plane hung up"),
        }
    }
}

impl Host for SocketHost<'_> {
    fn status(&self, verb: &str, message: &str) {
        // Losing a status line isn't worth failing over; a dead connection
        // shows up at the next call that returns something.
        let _ = self.call::<Value>(Message::Status {
            verb: verb.into(),
            message: message.into(),
        });
    }

    fn warn(&self, message: &str) {
        let _ = self.call::<Value>(Message::Warn {
            message: message.into(),
        });
    }

    fn interactive(&self) -> bool {
        self.call(Message::Interactive).unwrap_or(false)
    }

    fn ask(&self, question: &str, default: Option<&str>) -> Result<String> {
        self.call(Message::Ask {
            question: question.into(),
            default: default.map(Into::into),
        })
    }

    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        self.call(Message::Confirm {
            question: question.into(),
            default,
        })
    }

    fn save_config(&self, config: &Value) -> Result<()> {
        self.call::<Value>(Message::SaveConfig {
            config: config.clone(),
        })
        .map(drop)
    }

    fn run_remote(&self, command: &str, capture: bool) -> Result<RemoteOutput> {
        self.call(Message::RunRemote {
            command: command.into(),
            capture,
        })
    }
}
