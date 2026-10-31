//! Synchronous adapter for the FigCraft server management and live-session protocols.

mod client;
mod error;

pub use client::{Access, Backend, Client, Export, Link};
pub use error::Error;

#[cfg(test)]
mod tests;
