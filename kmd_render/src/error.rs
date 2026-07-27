//! The one driver-level error this crate actually carries.
//!
//! This used to be a five-variant `DriverError` enum with an `into_ntstatus`
//! table, a `From` impl and a `status_of` helper. Four of the five variants
//! (`InsufficientResources`, `InvalidParameter`, `IoError`, `NotImplemented`)
//! were NEVER CONSTRUCTED anywhere in the crate, so the table read as a
//! driver-wide error-to-NTSTATUS policy while the only value it could ever map
//! was `DeviceNotFound`.
//!
//! That mattered, because the illusion of a policy invites using it. Every DDI
//! in this driver chooses its own NTSTATUS from its own documented legal return
//! set -- `STATUS_DEVICE_DOES_NOT_EXIST` is not in most of them, which is
//! exactly why real DDIs ignore this type and return their own status (e.g.
//! `submit_command.rs` returns `STATUS_DEVICE_NOT_READY`). T6/R917.
//!
//! The transport keeps its own separate `VirtioError` (`virtio/mod.rs`).

use crate::dxgk::*;

/// The adapter has no started state: no `dxgkrnl` interface, or no transport.
///
/// Returned by the three `AdapterContext` accessors that reach into the started
/// state. One shape, one meaning, one status. Once T3's `StartedState` work is
/// finished those accessors become `Option<&StartedState>` and this type
/// disappears entirely.
#[derive(Debug, Clone, Copy)]
pub struct NotStarted;

impl From<NotStarted> for NTSTATUS {
    fn from(_: NotStarted) -> Self {
        STATUS_DEVICE_DOES_NOT_EXIST
    }
}
