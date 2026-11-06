//! Liquid and Solana plumbing shared by the CLI and the maker.

#![forbid(unsafe_code)]

#[cfg(feature = "native")]
pub mod esplora;
pub mod liquid;
#[cfg(feature = "native")]
pub mod sol;
pub mod swap;
