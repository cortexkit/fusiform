#![forbid(unsafe_code)]

//! The ck-fusiform daemon's logic, as a library.
//!
//! The binary is a thin `main` over this crate rather than the other way
//! around, so integration tests link the same code the daemon runs. The
//! alternative — a binary-only crate with tests pulling modules in by path —
//! compiles every module twice, once per target, and each copy sees the other's
//! exports as unused.

pub mod fetch;
pub mod health;
pub mod loop_;
pub mod signals;
