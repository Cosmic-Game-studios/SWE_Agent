//! The core of SWE Agent: the agent loop, sessions, sub-agents and
//! everything they need (configuration, permissions, prompts, compaction).
//!
//! The entry point is [`SessionManager`]: open it on an agent home directory,
//! create sessions, send messages, and follow what happens through each
//! session's event stream.

mod agent;
mod checkpoint;
mod compaction;
pub mod config;
mod diagnostics;
mod external;
pub mod mcp;
pub mod permissions;
pub mod prompt;
pub mod session;
mod subagent;
mod worktree;

pub use checkpoint::Checkpoint;
pub use config::{Config, DEMO_PROVIDER, KeyStatus, Paths, ProviderConfig, ProviderInfo, ProviderRegistry};
pub use external::{EngineInfo, engines};
pub use session::{
    CreateSession, GlobalEvent, Session, SessionError, SessionManager, SessionSnapshot, UpdateSession, UserInput,
};
