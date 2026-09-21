//! `unisoc-cpd` — a device-independent control daemon for the Unisoc CP.
//!
//! The AP-side contract is a property of the baseband *generation*, not of the
//! board, so the core here never names a platform: channel nodes, partitions,
//! spool names, interface names and vendor commands all come from a profile.
//! `tools/profile-check` fails the build when that boundary is crossed.

pub mod at;
pub mod capability;
pub mod channel;
pub mod cli;
pub mod context;
pub mod identity;
pub mod nat;
pub mod pdu;
pub mod probes;
pub mod profile;
pub mod profile_check;
pub mod telemetry;
pub mod unisoc_at;
pub mod urc;

pub use context::Mode;
