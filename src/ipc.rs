// The manager's control channel. On Unix it is a socket in the private home directory.
// Windows has no socket files there, so the manager serves a named pipe with an
// unguessable name and owner-only DACL, and records the name in the home directory.
use crate::config::Paths;
use anyhow::Result;
#[cfg(unix)]
pub use unix::{Listener, Server, connect};
#[cfg(windows)]
pub use windows::{Listener, Server, connect};

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::Context;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio::net::{UnixListener, UnixStream};

    pub type Server = UnixStream;
    pub struct Listener(UnixListener);
    impl Listener {
        pub fn bind(paths: &Paths) -> Result<Self> {
            if paths.socket().exists() {
                fs::remove_file(paths.socket())?;
            }
            // Socket paths are limited to about 100 bytes (104 on macOS, 108 on Linux).
            let listener = UnixListener::bind(paths.socket()).with_context(|| {
                format!(
                    "cannot listen at {}; use a shorter --home path",
                    paths.socket().display()
                )
            })?;
            fs::set_permissions(paths.socket(), fs::Permissions::from_mode(0o600))?;
            Ok(Self(listener))
        }
        pub async fn accept(&mut self) -> Result<Server> {
            Ok(self.0.accept().await?.0)
        }
    }
    pub async fn connect(paths: &Paths) -> std::io::Result<UnixStream> {
        UnixStream::connect(paths.socket()).await
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::windows::Descriptor;
    use std::{io, time::Duration};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    const PREFIX: &str = r"\\.\pipe\runnerctl-";

    pub type Server = NamedPipeServer;
    pub struct Listener {
        name: String,
        descriptor: Descriptor,
        next: NamedPipeServer,
    }
    fn create(name: &str, descriptor: &Descriptor, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = descriptor.attributes();
        // SAFETY: the attributes and their descriptor outlive the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    std::ptr::from_mut(&mut attributes).cast(),
                )
        }
    }
    impl Listener {
        pub fn bind(paths: &Paths) -> Result<Self> {
            let name = format!("{PREFIX}{}", uuid::Uuid::new_v4().simple());
            let descriptor = Descriptor::private()?;
            let next = create(&name, &descriptor, true)?;
            crate::config::atomic_write(&paths.socket(), name.as_bytes())?;
            Ok(Self {
                name,
                descriptor,
                next,
            })
        }
        pub async fn accept(&mut self) -> Result<Server> {
            self.next.connect().await?;
            // Keep an instance listening before handing this one to the connection.
            let next = create(&self.name, &self.descriptor, false)?;
            Ok(std::mem::replace(&mut self.next, next))
        }
    }
    pub async fn connect(paths: &Paths) -> io::Result<NamedPipeClient> {
        let name = std::fs::read_to_string(paths.socket())?;
        // Never let a damaged file redirect the client to an arbitrary path.
        if !name.starts_with(PREFIX) || name.len() != PREFIX.len() + 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid manager pipe name",
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match ClientOptions::new().open(&name) {
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                result => return result,
            }
        }
    }
}
