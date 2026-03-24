//! Pressure system — handle PSI (Pressure Stall Information) events.
//!
//! When a service's cgroup memory or CPU pressure exceeds the configured
//! threshold, Tag::Psi(id) fires. For now, we log the event. Future work:
//! throttle, kill, or notify the operator.
//!
//! Zero unsafe in this file.

use crate::world::{ServiceId, World};

/// Handle a PSI event for a service. Called when Tag::Psi(id) fires.
pub fn on_pressure(world: &World, id: ServiceId) {
    let name = &world.names[id];
    eprintln!("arkhd: pressure: memory pressure detected for {name}");
}
