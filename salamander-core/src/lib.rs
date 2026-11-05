//! Shared GTK-free Salamander RS engine.  Used by both the desktop and the SSH terminal UI.
#![cfg(unix)]

pub mod archive;
pub mod attrs;
pub mod config;
pub mod find;
pub mod model;
pub mod ops;
pub mod remote;
pub mod sodium;
pub mod sys;
pub mod vault;
pub mod xattr;
