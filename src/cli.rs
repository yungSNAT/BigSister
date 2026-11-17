//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Find where your coding agent's tokens go, from its own transcripts.
///
/// Reads Claude Code, Codex and pi transcripts on this machine and prints names,
/// counts and sizes. Never prints message text, tool inputs or tool outputs.
#[derive(Parser, Debug)]
#[command(name = "shaphan", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Count records at or after this time: 7d, 24h, YYYY-MM-DD (local midnight) or an RFC 3339 time
    #[arg(long, global = true, default_value = "7d", value_name = "WHEN")]
    pub since: String,

    /// Which agent's transcripts to read
    #[arg(long, global = true, value_enum, default_value_t = AgentChoice::All)]
    pub agent: AgentChoice,

    /// Claude Code transcript folder [default: ~/.claude/projects]
    #[arg(long, global = true, value_name = "DIR")]
    pub claude_root: Option<PathBuf>,

    /// Codex transcript folder [default: ~/.codex/sessions and ~/.codex/archived_sessions]
    #[arg(long, global = true, value_name = "DIR")]
    pub codex_root: Option<PathBuf>,

    /// pi session folder [default: ~/.pi/agent/sessions]
    #[arg(long, global = true, value_name = "DIR")]
    pub pi_root: Option<PathBuf>,

    /// Print one JSON object instead of tables
    #[arg(long, global = true)]
    pub json: bool,

    /// Print the values --json prints for the detectors as one CSV table, a value a record
    #[arg(long, global = true, conflicts_with = "json")]
    pub csv: bool,

    /// Most rows to print in each table
    #[arg(long, global = true, default_value_t = 20, value_name = "N")]
    pub limit: usize,

    /// Also list re-read file paths (rereads only)
    #[arg(long, global = true)]
    pub paths: bool,

    /// Price every token estimate, and cache writes at their write multipliers, at this one rate in US dollars per million tokens, instead of each model's price (above 0, up to 1000)
    #[arg(long, global = true, value_name = "USD", value_parser = parse_usd_per_mtok)]
    pub usd_per_mtok: Option<f64>,

    /// Price token estimates from this price file instead of the bundled prices: a JSON object of model ids, each with input_cost_per_token in US dollars (LiteLLM's names)
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        conflicts_with = "usd_per_mtok"
    )]
    pub prices: Option<PathBuf>,
}

/// A price per million tokens: a finite number above 0 and at most 1000.
fn parse_usd_per_mtok(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(usd) if usd > 0.0 && usd <= 1000.0 => Ok(usd),
        _ => Err("expected a number above 0 and at most 1000".to_owned()),
    }
}

/// What to report.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Every detector, in one pass
    All,
    /// What loads before the first non-synthetic reply of each main session (Claude)
    Startup,
    /// Tool schemas loaded against tools called, and skills listed against skills used (Claude)
    Schemas,
    /// Test runs whose exit status a pipe into tail or head hides (Claude, Codex, pi)
    Pipes,
    /// Commands repeated with no edit in between (Claude, Codex, pi)
    Polls,
    /// Files read again after a context compaction (Claude, Codex, pi)
    Rereads,
    /// Tool calls by tool: result sizes, errors, error cascades and retry churn (Claude, pi)
    Tools,
    /// Cache writes by cause: new context, or rebuilds after an idle gap, a model switch or otherwise (Claude)
    Cache,
    /// Input-side cost by model, reasoning effort and subagent type, and subagents on their parent's top-priced model (Claude)
    Models,
    /// Hook durations, cancellations and timeouts (Claude)
    Hooks,
}

/// Agent selection for `--agent`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentChoice {
    Claude,
    Codex,
    Pi,
    All,
}
