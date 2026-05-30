// Transport backends: where a port's bytes actually flow.
//
// The default backend is a kernel PTY (`openpty`): the service holds the
// master, the client opens the printed slave path. A "virtual USB" serial
// device (CDC-ACM) enumerates as an ordinary tty, so the `tty` backend
// simply binds an existing device path (e.g. `/dev/ttyACM0`, `/dev/ttyGS0`)
// — the matching/framing logic above this layer is identical.
//
// `open` returns an `Opened` bundle that decouples `PortState` from any
// one backend: a read half (with fd access for idle-timeout polling), a
// write half, the client-visible device path, and an optional keepalive
// fd (a PTY-specific quirk, see below).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use nix::pty::openpty;
use nix::unistd::ttyname;
use serde::Deserialize;

/// A readable source that also exposes its raw fd, so the reader thread
/// can `poll` it for idle-timeout framing.
pub trait ReadSource: Read + Send {
    fn raw_fd(&self) -> RawFd;
}

impl ReadSource for File {
    fn raw_fd(&self) -> RawFd {
        self.as_raw_fd()
    }
}

/// The result of opening a transport: the two byte-stream halves plus
/// metadata `PortState` needs.
pub struct Opened {
    pub reader: Box<dyn ReadSource>,
    pub writer: Box<dyn Write + Send>,
    /// Client-visible device path to advertise/log, if any.
    pub device_path: Option<PathBuf>,
    /// An fd kept open only to hold a kernel resource alive for the port's
    /// lifetime. For a PTY this is the slave end: on macOS the master
    /// `read()` returns EIO/EOF whenever no fd anywhere holds the slave,
    /// so we keep a dummy reference and the master blocks like a real
    /// serial port until an external consumer attaches. `None` for tty.
    pub keepalive: Option<OwnedFd>,
}

/// YAML config for a port's transport. `type` selects the backend;
/// absent defaults to `pty`.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportConfig {
    /// Allocate a fresh kernel PTY (the historical default).
    #[default]
    Pty,
    /// Bind an existing tty device path — e.g. a USB-serial (CDC-ACM)
    /// device that already enumerates at `/dev/ttyACM0` or `/dev/ttyGS0`.
    Tty { path: PathBuf },
}

/// Open the configured transport.
pub fn open(cfg: &TransportConfig) -> std::io::Result<Opened> {
    match cfg {
        TransportConfig::Pty => open_pty(),
        TransportConfig::Tty { path } => open_tty(path),
    }
}

fn open_pty() -> std::io::Result<Opened> {
    let pty = openpty(None, None).map_err(io_err)?;
    let pty_path = ttyname(&pty.slave).map_err(io_err)?;
    let keepalive = pty.slave;

    let master_fd = pty.master.into_raw_fd();
    // SAFETY: master_fd was just produced by into_raw_fd and is owned here.
    let writer = unsafe { File::from_raw_fd(master_fd) };
    // Independent handle so the reader can read while triggers hold the
    // write mutex.
    let reader = writer.try_clone()?;

    Ok(Opened {
        reader: Box::new(reader),
        writer: Box::new(writer),
        device_path: Some(pty_path),
        keepalive: Some(keepalive),
    })
}

fn open_tty(path: &PathBuf) -> std::io::Result<Opened> {
    let writer = OpenOptions::new().read(true).write(true).open(path)?;
    let reader = writer.try_clone()?;
    Ok(Opened {
        reader: Box::new(reader),
        writer: Box::new(writer),
        device_path: Some(path.clone()),
        keepalive: None,
    })
}

fn io_err(e: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_opens_with_path_and_keepalive() {
        let opened = open(&TransportConfig::Pty).unwrap();
        assert!(
            opened.device_path.is_some(),
            "PTY must advertise a slave path"
        );
        assert!(
            opened.keepalive.is_some(),
            "PTY must hold the slave keepalive"
        );
        assert!(opened.reader.raw_fd() >= 0);
    }

    #[test]
    fn tty_binds_existing_device_path() {
        // Stand up a PTY and treat its slave as an "existing tty device",
        // which is exactly how a USB-serial (CDC-ACM) port would appear.
        let pty = open(&TransportConfig::Pty).unwrap();
        let path = pty.device_path.clone().unwrap();

        let opened = open(&TransportConfig::Tty { path: path.clone() }).unwrap();
        assert_eq!(opened.device_path.as_deref(), Some(path.as_path()));
        assert!(opened.keepalive.is_none(), "tty backend has no keepalive");
        assert!(opened.reader.raw_fd() >= 0);
    }

    #[test]
    fn tty_missing_path_errors() {
        match open(&TransportConfig::Tty {
            path: "/nonexistent/tty-device-xyz".into(),
        }) {
            Ok(_) => panic!("expected open to fail for a missing path"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }
}
