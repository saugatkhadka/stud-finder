//! The `ripple` command: indexing engine, daemon, client and CLI.
//!
//! The binary is a thin wrapper around [`cli::main`]; everything is here so
//! it can be tested in-process.

pub mod bench;
pub mod cli;
pub mod client;
pub mod edgefile;
pub mod engine;
pub mod files;
pub mod git;
pub mod gitd;
pub mod historyd;
pub mod output;
pub mod paths;
pub mod protocol;
pub mod query;
pub mod server;
pub mod signals;
