//! wang-tokens: measure every model call, then optimize.
//!
//! - [`pricing`]: per-model $/MTok and cost of a call
//! - [`cap`]: tool-output caps (keep head + tail)
//! - [`ledger`]: SQLite log of tasks, model calls and tool calls, plus summaries
//! - [`proxy`]: what `wang proxy` changed in each request it passed through

pub mod cap;
pub mod ledger;
pub mod pricing;
pub mod proxy;
pub mod stats;

pub use cap::{Capped, cap_middle};
pub use ledger::{CallRecord, Ledger, Summary, ToolRecord};
pub use stats::Stats;
pub use proxy::{ProxyRecord, ProxySummary};
pub use pricing::{Price, Usage, cost_usd, price_for};
