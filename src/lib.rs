#![warn(missing_docs)]

//! # aur-depend
//!
//! aur-depends is a dependency solving library for the AUR
//!
//! See [`Resolver`](struct.Resolver.html) for more info.

mod actions;
mod error;
mod resolve;
mod satisfies;

#[cfg(test)]
mod tests;

pub use crate::actions::*;
pub use crate::error::*;
pub use crate::resolve::*;
