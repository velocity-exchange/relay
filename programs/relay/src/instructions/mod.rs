//! One file per instruction; context struct at the top, handler below.

pub mod assert_paid_v0;
pub mod begin_guard_v0;
pub mod close_watch_v0;
pub mod register_watch_v0;

pub use assert_paid_v0::*;
pub use begin_guard_v0::*;
pub use close_watch_v0::*;
pub use register_watch_v0::*;
