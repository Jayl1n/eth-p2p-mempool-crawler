//! BSC P2P support module
//!
//! This module contains BSC-specific implementations for:
//! - Chain specification (chainspec.rs)
//! - P2P handshake protocol (handshake.rs)
//! - Upgrade status message (upgrade_status.rs)

mod chainspec;
mod handshake;
mod upgrade_status;

pub use chainspec::{bsc_chain_spec, boot_nodes, head};
pub use handshake::BscHandshake;