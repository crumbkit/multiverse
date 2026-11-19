//! mxv-services serves the local AWS services of a workspace in one
//! process. Each [`Kind`] listens on its own port on 127.0.0.1 and keeps its
//! data in memory, so the data is gone when the process stops.
//!
//! `mxv` runs it as the host app `mxv-services`, with the command line
//! `mxv phase services <kind>=<port>...`, which [`parse`] reads. [`serve`]
//! binds every listener, calls its `ready` callback once all of them are
//! bound, and serves until its stop future resolves, which [`signals`]
//! gives for TERM and INT.
//!
//! The `dynamodb` kind is Dynoxide, embedded as a library, on an in-memory
//! SQLite database. Dynoxide never deletes an item whose time-to-live
//! attribute has passed, so expired items stay until the process stops.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use tokio::sync::{mpsc, watch};

#[cfg(test)]
mod tests;

/// A kind of service that mxv-services serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// DynamoDB, served by Dynoxide.
    Dynamodb,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Self; 1] = [Self::Dynamodb];

    /// Returns the kind as the command line writes it, such as `dynamodb`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dynamodb => "dynamodb",
        }
    }

    /// Returns the kind that the command line writes as `text`, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One listener: a kind and the port on 127.0.0.1 it listens on. Port 0
/// lets the operating system choose one, which only a test asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listener {
    /// The kind it serves.
    pub kind: Kind,
    /// The port on 127.0.0.1.
    pub port: u16,
}

/// A listener once bound, with the address it listens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    /// The kind it serves.
    pub kind: Kind,
    /// The address it listens on.
    pub address: SocketAddr,
}

/// Why the command line names no listeners that [`serve`] can run.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArgumentError {
    /// The command line names no listener.
    Empty,
    /// A word is not `<kind>=<port>` with a known kind and a port from 1
    /// to 65535.
    Malformed {
        /// The word.
        word: String,
    },
    /// Two words name one kind.
    Repeated {
        /// The kind.
        kind: Kind,
    },
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str(
                "the command takes one `<kind>=<port>` argument for each kind it serves",
            ),
            Self::Malformed { word } => {
                let kinds: Vec<&str> = Kind::ALL.iter().map(|kind| kind.as_str()).collect();
                write!(
                    formatter,
                    "`{word}` is not `<kind>=<port>`, where the kind is one of {} and the port is from 1 to 65535",
                    kinds.join(", ")
                )
            }
            Self::Repeated { kind } => write!(formatter, "the kind `{kind}` is named twice"),
        }
    }
}

impl std::error::Error for ArgumentError {}

/// Returns the listeners that `words`, the arguments after
/// `mxv phase services`, name: one `<kind>=<port>` for each kind.
///
/// # Errors
///
/// Returns [`ArgumentError`] when `words` is empty, a word is not
/// `<kind>=<port>`, or two words name one kind.
pub fn parse(words: &[OsString]) -> Result<Vec<Listener>, ArgumentError> {
    if words.is_empty() {
        return Err(ArgumentError::Empty);
    }
    let mut listeners: Vec<Listener> = Vec::new();
    for word in words {
        let text = word.to_string_lossy();
        let listener = text
            .split_once('=')
            .and_then(|(kind, port)| {
                Some(Listener {
                    kind: Kind::parse(kind)?,
                    port: port.parse::<u16>().ok().filter(|port| *port != 0)?,
                })
            })
            .ok_or_else(|| ArgumentError::Malformed {
                word: text.clone().into_owned(),
            })?;
        if listeners.iter().any(|known| known.kind == listener.kind) {
            return Err(ArgumentError::Repeated {
                kind: listener.kind,
            });
        }
        listeners.push(listener);
    }
    Ok(listeners)
}

/// Why [`serve`] stopped with a failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ServeError {
    /// The runtime, or the stop future, could not be set up.
    Setup {
        /// The error.
        error: io::Error,
    },
    /// A listener could not bind its port, such as one that another
    /// program holds.
    Bind {
        /// The listener's kind.
        kind: Kind,
        /// The port.
        port: u16,
        /// The error.
        error: io::Error,
    },
    /// A listener's server failed while it served.
    Serve {
        /// The listener's kind.
        kind: Kind,
        /// The server's message.
        message: String,
    },
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup { error } => write!(formatter, "the server could not start: {error}"),
            Self::Bind { kind, port, error } => write!(
                formatter,
                "{kind} could not listen on 127.0.0.1:{port}: {error}"
            ),
            Self::Serve { kind, message } => write!(formatter, "{kind} stopped: {message}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// Returns a future that resolves when the process receives TERM or INT,
/// which phase's keeper sends to stop the app. It must be called inside a
/// Tokio runtime, as [`serve`] calls its `stop` argument.
///
/// # Errors
///
/// Returns the error of registering either signal.
pub fn signals() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(std::future::poll_fn(move |context| {
        if term.poll_recv(context).is_ready() || interrupt.poll_recv(context).is_ready() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }))
}

/// Binds every listener of `listeners` on 127.0.0.1, calls `ready` with
/// their addresses once all of them are bound, and serves until the future
/// that `stop` returns resolves or a server fails. On stop each server
/// finishes the requests it is answering and then returns. `stop` is
/// called inside the runtime before any listener binds, so a signal that
/// arrives after `ready` is never missed.
///
/// # Errors
///
/// Returns [`ServeError::Setup`] when the runtime or `stop` fails,
/// [`ServeError::Bind`] when a listener cannot bind, before `ready` is
/// called, and [`ServeError::Serve`] when a server fails.
pub fn serve<F>(
    listeners: &[Listener],
    ready: impl FnOnce(&[Bound]),
    stop: impl FnOnce() -> io::Result<F>,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|error| ServeError::Setup { error })?;
    runtime.block_on(async move {
        let stop = stop().map_err(|error| ServeError::Setup { error })?;
        let mut bound = Vec::new();
        for listener in listeners {
            let socket = tokio::net::TcpListener::bind(SocketAddr::from((
                Ipv4Addr::LOCALHOST,
                listener.port,
            )))
            .await
            .and_then(|socket| Ok((socket.local_addr()?, socket)))
            .map_err(|error| ServeError::Bind {
                kind: listener.kind,
                port: listener.port,
                error,
            })?;
            bound.push((listener.kind, socket));
        }
        let addresses: Vec<Bound> = bound
            .iter()
            .map(|(kind, (address, _))| Bound {
                kind: *kind,
                address: *address,
            })
            .collect();
        ready(&addresses);
        run(bound, stop).await
    })
}

/// Serves each bound listener until `stop` resolves or one server fails,
/// and then stops the others and waits for them.
async fn run(
    bound: Vec<(Kind, (SocketAddr, tokio::net::TcpListener))>,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let (stopping, stopped) = watch::channel(false);
    let (done, mut finished) = mpsc::unbounded_channel();
    let count = bound.len();
    for (kind, (_, socket)) in bound {
        let done = done.clone();
        let mut stopped = stopped.clone();
        let shutdown = async move {
            drop(stopped.wait_for(|stop| *stop).await);
        };
        drop(tokio::spawn(async move {
            let result = match kind {
                Kind::Dynamodb => dynamodb(socket, shutdown).await,
            };
            drop(done.send((kind, result)));
        }));
    }
    // Only the servers hold a sender now, so the loop below ends when the
    // last one returns, even when one ends without sending.
    drop(done);
    // `stopped` lives until `run` returns, so a send to `stopping` never
    // fails while a server runs, and its result says nothing.
    let on_stop = stopping.clone();
    drop(tokio::spawn(async move {
        stop.await;
        let _ = on_stop.send(true);
    }));
    let mut failure = None;
    for _ in 0..count {
        let Some((kind, result)) = finished.recv().await else {
            break;
        };
        if let Err(message) = result {
            failure.get_or_insert(ServeError::Serve { kind, message });
        }
        // A server that returns, failed or not, stops the others, so the
        // process exits and phase's keeper restarts it whole.
        let _ = stopping.send(true);
    }
    failure.map_or(Ok(()), Err)
}

/// Serves the DynamoDB API from a new in-memory database on `socket` until
/// `shutdown` resolves.
async fn dynamodb(
    socket: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let database = dynoxide::Database::memory().map_err(|error| error.to_string())?;
    dynoxide::server::serve_with_shutdown(socket, database, shutdown).await
}
