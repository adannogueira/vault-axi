//! I/O-free core shared by the vault-axi CLI (native) and the Obsidian plugin (WASM).
//!
//! Nothing in this crate touches the filesystem, network, clock or a database: callers pass
//! data in and execute the returned decisions with their own I/O.

pub mod crypto;
pub mod hash;
pub mod merge;
pub mod paths;
pub mod payload;
