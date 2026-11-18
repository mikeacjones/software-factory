//! A live page of what the factory is doing.
//!
//! It is observability, not a part of the factory: it reads the records
//! the factory keeps under its state directory and serves one page from
//! them. It uses no factory crate, writes nothing anywhere, and answers
//! only the networks it is told to.

pub mod act;
pub mod inbox;
pub mod net;
pub mod patch;
pub mod records;
pub mod runlog;
pub mod server;
pub mod thread;
