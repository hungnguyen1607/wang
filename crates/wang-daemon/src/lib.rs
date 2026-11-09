//! wang daemon library: agent loop, model clients, tools, and channels.
//! The `wang` binary lives in `wang-cli`.

pub mod agent;
pub mod approvals;
pub mod config;
pub mod daemon;
pub mod mcp;
pub mod memory;
pub mod model;
pub mod notify;
pub mod orchestrate;
pub mod protocol;
pub mod schedule;
pub mod server;
pub mod telegram;
pub mod tools;
pub mod workflow;

pub use agent::{Agent, Conversation};
pub use config::Config;
