//! Shared types for SWE Agent.
//!
//! This crate has no I/O and no async: it only defines the data that flows
//! between the LLM adapters, the agent loop, the HTTP API and the UI.

pub mod event;
pub mod message;

pub use event::*;
pub use message::*;
