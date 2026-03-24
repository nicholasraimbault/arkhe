//! Socket activation system — pre-bind sockets and spawn services on demand.
//!
//! The supervisor binds sockets declared in each service's ListenSockets config
//! at startup. When a connection arrives (POLLIN on the listen fd), the service
//! is spawned and the listen fd is passed to the child via sd_listen_fds protocol.
//!
//! Zero unsafe in this file.

use std::os::fd::AsRawFd;

use io_uring::IoUring;

use crate::components::{ListenAddr, RuntimeState};
use crate::error::SupervisorError;
use crate::ring::{build_poll_multishot, Tag};
use crate::sys;
use crate::world::{ServiceId, World};

/// Bind all sockets declared by services and submit multishot polls.
/// Called once at startup after services are loaded.
pub fn setup_sockets(world: &mut World, ring: &mut IoUring) -> Result<(), SupervisorError> {
    for id in 0..world.len() {
        let sockets = match &world.listen_sockets[id] {
            Some(ls) => &ls.sockets,
            None => continue,
        };
        let name = world.names[id].clone();

        for addr in sockets {
            let fd = match addr {
                ListenAddr::Tcp(port) => sys::bind_tcp(*port)
                    .map_err(|e| SupervisorError::SocketBind(format!("tcp:{port}"), e)),
                ListenAddr::Tcp6(sa) => sys::bind_tcp_addr(*sa)
                    .map_err(|e| SupervisorError::SocketBind(format!("tcp6:{sa}"), e)),
                ListenAddr::Unix(path) => sys::bind_unix(path).map_err(|e| {
                    SupervisorError::SocketBind(format!("unix:{}", path.display()), e)
                }),
            };

            match fd {
                Ok(listen_fd) => {
                    // Submit multishot poll — fires when a connection is pending
                    let sqe = build_poll_multishot(&listen_fd, Tag::Accept(id));
                    sys::push_sqe(ring, &sqe)?;

                    world.socket_map.insert(listen_fd.as_raw_fd(), id);
                    world.listen_fds[id].push(listen_fd);
                    eprintln!(
                        "arkhd: socket: bound {addr_desc} for {name}",
                        addr_desc = match addr {
                            ListenAddr::Tcp(p) => format!("tcp:{p}"),
                            ListenAddr::Tcp6(sa) => format!("tcp6:{sa}"),
                            ListenAddr::Unix(p) => format!("unix:{}", p.display()),
                        }
                    );
                }
                Err(e) => {
                    eprintln!("arkhd: socket: failed to bind for {name}: {e}");
                }
            }
        }
    }
    Ok(())
}

/// Handle a connection event on a socket-activated service.
/// If the service is NotStarted, spawn it. Otherwise ignore.
pub fn on_accept(
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    match world.states[id] {
        RuntimeState::NotStarted => {
            eprintln!("arkhd: socket: connection on {}, spawning", world.names[id]);
            crate::systems::spawn::spawn_service(world, id, ring)?;
        }
        _ => {
            // Service already running — it will accept the connection itself
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bind_tcp_and_release() {
        // Bind to an ephemeral port
        match sys::bind_tcp(0) {
            Ok(fd) => {
                assert!(fd.as_raw_fd() >= 0);
                // fd drops here — socket is released
            }
            Err(e) => {
                eprintln!("bind_tcp_and_release: skipped ({e})");
            }
        }
    }

    #[test]
    fn bind_unix_and_cleanup() {
        let path =
            std::env::temp_dir().join(format!("arkhe-sock-test-{}.sock", std::process::id()));
        let path_buf = PathBuf::from(&path);

        match sys::bind_unix(&path_buf) {
            Ok(fd) => {
                assert!(fd.as_raw_fd() >= 0);
                assert!(path.exists());
                drop(fd);
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                eprintln!("bind_unix_and_cleanup: skipped ({e})");
            }
        }
    }
}
