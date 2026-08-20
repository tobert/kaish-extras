//! An `AF_UNIX` transport for ureq, so `--unix-socket` connects to a socket
//! instead of to the URL's host.
//!
//! ureq 3.x has no first-class unix-socket connect, so this reaches through
//! `ureq::unversioned::transport`, which carries **no semver guarantee** —
//! docs/issues.md CU7 records that, and the ureq pin exists to bound it. The
//! shape is deliberately a copy of ureq's own `TcpTransport`: the same buffer
//! handling and the same timeout handling, over `UnixStream` rather than
//! `TcpStream`, so there is one place to look when ureq changes.
//!
//! The path arrives already resolved and containment-checked — see
//! `tool.rs`, which maps it through `ToolCtx::resolve_path` and
//! `backend().resolve_real_path()` before the request is built. This module
//! opens what it is given and nothing else.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use ureq::Error;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};

/// Connects every request to one filesystem socket.
///
/// The URI's host is ignored, exactly as `curl --unix-socket` ignores it —
/// the host in `http://localhost/info` is a placeholder that still has to be
/// spelled, because HTTP needs a `Host` header.
#[derive(Debug)]
pub struct UnixConnector {
    path: PathBuf,
}

impl UnixConnector {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl<In: Transport> Connector<In> for UnixConnector {
    type Out = UnixTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        let stream = UnixStream::connect(&self.path).map_err(Error::Io)?;
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(UnixTransport {
            stream,
            buffers,
            timeout_read: None,
            timeout_write: None,
        }))
    }
}

/// A ureq [`Transport`] over a connected [`UnixStream`].
#[derive(Debug)]
pub struct UnixTransport {
    stream: UnixStream,
    buffers: LazyBuffers,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
}

/// Set a socket timeout only when it changed, to avoid a syscall per call —
/// the same reasoning ureq's `TcpTransport` applies.
fn maybe_update_timeout(
    timeout: NextTimeout,
    previous: &mut Option<Duration>,
    stream: &UnixStream,
    apply: impl Fn(&UnixStream, Option<Duration>) -> io::Result<()>,
) -> io::Result<()> {
    let wanted = timeout.not_zero().map(|t| *t);
    if wanted != *previous {
        apply(stream, wanted)?;
        *previous = wanted;
    }
    Ok(())
}

/// A non-blocking socket reports `WouldBlock` where a timeout means
/// `TimedOut`. ureq's own transports normalize this through a private helper;
/// this is that helper, spelled out.
fn normalize(result: io::Result<usize>) -> io::Result<usize> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            Err(io::Error::new(io::ErrorKind::TimedOut, e))
        }
        other => other,
    }
}

impl Transport for UnixTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        maybe_update_timeout(
            timeout,
            &mut self.timeout_write,
            &self.stream,
            UnixStream::set_write_timeout,
        )?;

        let output = &self.buffers.output()[..amount];
        match normalize(self.stream.write_all(output).map(|()| 0)) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(Error::Timeout(timeout.reason)),
            Err(e) => Err(e.into()),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        maybe_update_timeout(
            timeout,
            &mut self.timeout_read,
            &self.stream,
            UnixStream::set_read_timeout,
        )?;

        let input = self.buffers.input_append_buf();
        let amount = match normalize(self.stream.read(input)) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                return Err(Error::Timeout(timeout.reason));
            }
            Err(e) => return Err(e.into()),
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        // A peer that closed sends EOF, which surfaces on the next read. There
        // is no cheap peek for a unix socket the way `probe_tcp_stream` gives
        // one for TCP, so this reports open and lets the read find out — the
        // cost is one retry on a stale pooled connection, not a wrong answer.
        true
    }
}
