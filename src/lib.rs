//! buildq: a machine-wide queue and a shared, disk-budgeted target-dir pool
//! for cargo jobs started by many parallel agents. See README.md.

pub mod cargo;
pub mod config;
pub mod exec;
pub mod gc;
pub mod git;
pub mod history;
pub mod job;
pub mod lock;
pub mod pool;
pub mod queue;
pub mod remote;
pub mod shim;
pub mod util;
