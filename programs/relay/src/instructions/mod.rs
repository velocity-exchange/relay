//! One file per instruction; context struct at the top, handler below.

pub mod close_watch_v0;
pub mod crank_v0;
pub mod register_watch_v0;

pub use close_watch_v0::*;
pub use crank_v0::*;
pub use register_watch_v0::*;
