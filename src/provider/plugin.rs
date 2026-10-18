//! Providers in separate binaries, `cargo-plane-provider-<name>` on PATH.
//! See the `cargo-plane-provider` crate for the plugin side and the protocol.

use std::cell::{RefCell, RefMut};
use std::fs;
use std::io::{BufReader, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use cargo_plane_provider::protocol::{self, Message, Reply, Request};
use cargo_plane_provider::{AnyProvider, Host, Setup, Started};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub fn binary_name(provider: &str) -> String {
    format!("cargo-plane-provider-{provider}")
}

/// A plugin binary, started on first use and kept running until dropped.
pub struct Plugin {
    name: String,
    bin: PathBuf,
    session: RefCell<Option<Session>>,
}

impl Plugin {
    pub fn locate(name: &str) -> Option<Self> {
        let file = binary_name(name);
        let path = std::env::var_os("PATH")?;
        let bin = std::env::split_paths(&path)
            .map(|dir| dir.join(&file))
            .find(|bin| bin.is_file())?;
        Some(Self {
            name: name.into(),
            bin,
            session: RefCell::new(None),
        })
    }

    fn session(&self) -> Result<RefMut<'_, Session>> {
        let mut session = self.session.borrow_mut();
        if session.is_none() {
            *session = Some(Session::spawn(&self.name, &self.bin)?);
        }
        Ok(RefMut::map(session, |s| s.as_mut().unwrap()))
    }

    fn call<T: DeserializeOwned>(&self, request: Request, host: &dyn Host) -> Result<T> {
        let result = self.session()?.call(&request, host);
        let value = result.with_context(|| format!("provider plugin `{}`", self.name))?;
        serde_json::from_value(value)
            .with_context(|| format!("provider plugin `{}` sent an unexpected answer", self.name))
    }
}

impl AnyProvider for Plugin {
    fn up(&self, config: &Value, host: &dyn Host) -> Result<Started> {
        self.call(
            Request::Up {
                config: config.clone(),
            },
            host,
        )
    }

    fn down(&self, config: &Value, host: &dyn Host) -> Result<()> {
        self.call::<Value>(
            Request::Down {
                config: config.clone(),
            },
            host,
        )
        .map(drop)
    }

    fn status(&self, config: &Value, host: &dyn Host) -> Result<String> {
        self.call(
            Request::Status {
                config: config.clone(),
            },
            host,
        )
    }

    fn purge(&self, config: &Value, host: &dyn Host) -> Result<bool> {
        self.call(
            Request::Purge {
                config: config.clone(),
            },
            host,
        )
    }

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        self.call::<Value>(
            Request::Setup {
                setup: setup.clone(),
            },
            host,
        )
        .map(drop)
    }
}

/// A connection to a running plugin.
struct Session {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    child: Option<Child>,
}

impl Session {
    /// Start the plugin with a socket to connect to. The socket lives in a
    /// fresh directory only we can enter, so nothing else can connect first.
    fn spawn(name: &str, bin: &PathBuf) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("cargo-plane-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let socket = dir.join("plugin.sock");
        let connected = (|| {
            let listener = UnixListener::bind(&socket)?;
            listener.set_nonblocking(true)?;
            let mut child = Command::new(bin)
                .env(protocol::SOCKET_ENV, &socket)
                .spawn()
                .with_context(|| format!("starting {}", bin.display()))?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false)?;
                        return Ok((stream, child));
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                    Err(err) => return Err(err.into()),
                }
                if let Some(status) = child.try_wait()? {
                    bail!("{} exited ({status}) without connecting", bin.display());
                }
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("{} didn't connect within 10s", bin.display());
                }
                sleep(Duration::from_millis(10));
            }
        })();
        let _ = fs::remove_dir_all(&dir);
        let (stream, child) =
            connected.with_context(|| format!("starting provider plugin `{name}`"))?;
        Self::handshake(stream, Some(child))
            .with_context(|| format!("starting provider plugin `{name}`"))
    }

    fn handshake(stream: UnixStream, child: Option<Child>) -> Result<Self> {
        let mut session = Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            child,
        };
        match protocol::recv::<Message>(&mut session.reader)? {
            Some(Message::Hello { protocol, .. }) if protocol == protocol::VERSION => Ok(session),
            Some(Message::Hello { protocol, sdk }) => bail!(
                "it speaks plugin protocol {protocol} (cargo-plane-provider {sdk}), but this \
                 cargo-plane speaks {}; rebuild the plugin against cargo-plane-provider {}",
                protocol::VERSION,
                env!("CARGO_PKG_VERSION")
            ),
            Some(other) => bail!("expected a hello, got {other:?}"),
            None => bail!("it hung up before saying hello"),
        }
    }

    /// Send a request and answer the plugin's callbacks until it's done.
    fn call(&mut self, request: &Request, host: &dyn Host) -> Result<Value> {
        protocol::send(&mut self.writer, request).context("the plugin hung up")?;
        loop {
            let message = protocol::recv::<Message>(&mut self.reader)?
                .context("the plugin exited while handling a request")?;
            let reply = match message {
                Message::Done { result } => return Ok(result),
                Message::Failed { error } => return Err(anyhow!(error)),
                Message::Hello { .. } => bail!("unexpected hello in the middle of a request"),
                callback => match answer(callback, host) {
                    Ok(value) => Reply::Ok { value },
                    Err(err) => Reply::Err {
                        error: format!("{err:#}"),
                    },
                },
            };
            protocol::send(&mut self.writer, &reply).context("the plugin hung up")?;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the socket tells the plugin to exit.
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}

fn answer(callback: Message, host: &dyn Host) -> Result<Value> {
    Ok(match callback {
        Message::Status { verb, message } => {
            host.status(&verb, &message);
            Value::Null
        }
        Message::Warn { message } => {
            host.warn(&message);
            Value::Null
        }
        Message::Interactive => Value::Bool(host.interactive()),
        Message::Ask { question, default } => {
            Value::String(host.ask(&question, default.as_deref())?)
        }
        Message::Confirm { question, default } => Value::Bool(host.confirm(&question, default)?),
        Message::SaveConfig { config } => {
            host.save_config(&config)?;
            Value::Null
        }
        Message::RunRemote { command, capture } => {
            serde_json::to_value(host.run_remote(&command, capture)?)?
        }
        Message::Hello { .. } | Message::Done { .. } | Message::Failed { .. } => {
            unreachable!("handled by the caller")
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::thread;

    use anyhow::Result;
    use cargo_plane_provider::{Endpoint, Provider, RemoteOutput};
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Config {
        host: String,
    }

    /// Asks a question in `up`, fails in `down`.
    struct Fake;

    impl Provider for Fake {
        type Config = Config;

        fn up(&self, config: &Config, host: &dyn Host) -> Result<Endpoint> {
            host.status("Starting", &config.host);
            let user = host.ask("User", Some("root"))?;
            Ok(Endpoint {
                user: Some(user),
                ..Endpoint::new(&config.host)
            })
        }

        fn down(&self, _: &Config, _: &dyn Host) -> Result<()> {
            bail!("can't stop {}", "it")
        }

        fn status(&self, config: &Config, _: &dyn Host) -> Result<String> {
            Ok(format!("fake {}", config.host))
        }

        fn boot_timeout(&self, _: &Config) -> Duration {
            Duration::from_secs(7)
        }
    }

    #[derive(Default)]
    struct RecordingHost {
        log: RefCell<Vec<String>>,
    }

    impl Host for RecordingHost {
        fn status(&self, verb: &str, message: &str) {
            self.log.borrow_mut().push(format!("{verb} {message}"));
        }
        fn warn(&self, message: &str) {
            self.log.borrow_mut().push(format!("warning: {message}"));
        }
        fn interactive(&self) -> bool {
            true
        }
        fn ask(&self, question: &str, _: Option<&str>) -> Result<String> {
            self.log.borrow_mut().push(format!("asked {question}"));
            Ok("me".into())
        }
        fn confirm(&self, _: &str, default: bool) -> Result<bool> {
            Ok(default)
        }
        fn save_config(&self, _: &Value) -> Result<()> {
            bail!("not during this test")
        }
        fn run_remote(&self, _: &str, _: bool) -> Result<RemoteOutput> {
            bail!("not during this test")
        }
    }

    fn connected() -> (Session, thread::JoinHandle<Result<()>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let plugin = thread::spawn(move || cargo_plane_provider::serve_on(theirs, &Fake));
        (Session::handshake(ours, None).unwrap(), plugin)
    }

    #[test]
    fn requests_and_callbacks_round_trip() {
        let (mut session, plugin) = connected();
        let host = RecordingHost::default();
        let config = serde_json::json!({ "host": "box", "provider": "fake" });

        let started: Started = serde_json::from_value(
            session
                .call(
                    &Request::Up {
                        config: config.clone(),
                    },
                    &host,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(started.endpoint.host, "box");
        assert_eq!(started.endpoint.user.as_deref(), Some("me"));
        assert_eq!(started.boot_timeout, Duration::from_secs(7));
        assert_eq!(*host.log.borrow(), ["Starting box", "asked User"]);

        let status = session
            .call(
                &Request::Status {
                    config: config.clone(),
                },
                &host,
            )
            .unwrap();
        assert_eq!(status, "fake box");

        let err = session.call(&Request::Down { config }, &host).unwrap_err();
        assert_eq!(err.to_string(), "can't stop it");

        let err = session
            .call(
                &Request::Status {
                    config: serde_json::json!({}),
                },
                &host,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid provider config"), "{err}");

        drop(session);
        plugin.join().unwrap().unwrap();
    }
}
