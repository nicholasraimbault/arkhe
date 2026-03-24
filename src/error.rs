//! Shared error types for the supervisor.
//!
//! Two classes:
//! - Fatal: log + exit(1), PID 1 restarts us
//! - Operational: log, mark service failed, continue

use std::fmt;

/// All errors the supervisor can produce.
pub enum SupervisorError {
    // Fatal — supervisor must exit
    RingInit(std::io::Error),
    SignalSetup(std::io::Error),
    RingSubmit(std::io::Error),

    // Operational — log and continue
    SignalRead(std::io::Error),
    DirCreate(std::io::Error),
    ConfigLoad(String, String),           // (service_name, reason)
    CgroupCreate(String, std::io::Error), // (path, error)
    CgroupWrite(String, std::io::Error),  // (path, error)
    SpawnFork(String, std::io::Error),    // (service_name, error)
    PipeCreate(std::io::Error),
    FanotifySetup(std::io::Error),
    FanotifyRead(std::io::Error),
    Sandbox(String),
    LogWrite(String, std::io::Error), // (service_name, error)
    WaitId(std::io::Error),
    SocketBind(String, std::io::Error), // (description, error)
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RingInit(e) => write!(f, "io_uring init: {e}"),
            Self::SignalSetup(e) => write!(f, "signal setup: {e}"),
            Self::RingSubmit(e) => write!(f, "ring submit: {e}"),
            Self::SignalRead(e) => write!(f, "signal read: {e}"),
            Self::DirCreate(e) => write!(f, "directory create: {e}"),
            Self::ConfigLoad(name, reason) => write!(f, "config load [{name}]: {reason}"),
            Self::CgroupCreate(path, e) => write!(f, "cgroup create [{path}]: {e}"),
            Self::CgroupWrite(path, e) => write!(f, "cgroup write [{path}]: {e}"),
            Self::SpawnFork(name, e) => write!(f, "spawn [{name}]: {e}"),
            Self::PipeCreate(e) => write!(f, "pipe create: {e}"),
            Self::FanotifySetup(e) => write!(f, "fanotify setup: {e}"),
            Self::FanotifyRead(e) => write!(f, "fanotify read: {e}"),
            Self::Sandbox(msg) => write!(f, "sandbox: {msg}"),
            Self::LogWrite(name, e) => write!(f, "log write [{name}]: {e}"),
            Self::WaitId(e) => write!(f, "waitid: {e}"),
            Self::SocketBind(desc, e) => write!(f, "socket bind [{desc}]: {e}"),
        }
    }
}

impl fmt::Debug for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
