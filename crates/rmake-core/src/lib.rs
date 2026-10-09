//! Native build services, independent of concrete compilers and terminal parsing.
pub mod depfile;
pub mod discovery;
pub mod execute;
pub mod fingerprint;
pub mod manifest;
pub mod plan;
pub mod process;
pub mod project;
pub mod runtime;
pub mod scheduler;
pub mod state;
pub mod storage;
pub mod usage;
