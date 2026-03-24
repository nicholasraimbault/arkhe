//! Systems — functions that operate on World state
//!
//! Each system is a plain function: fn(&mut World, ...) → ()
//! No trait objects. No dynamic dispatch. No allocation in the hot path.
//! Systems are dispatched by the io_uring event loop based on completion tags.

pub mod spawn;
pub mod supervise;
pub mod deps;
pub mod log;
pub mod socket;
pub mod pressure;
pub mod status;
pub mod mounts;
