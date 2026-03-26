use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Built-in archetypes (with tailored prompts when using --anchor):
  security    Auth boundaries, injection, secrets, trust assumptions
  bugs        Logic errors, edge cases, error handling, crashes
  perf        Allocations, complexity, hot paths, async blocking
  arch        Coupling, abstractions, API design, consistency

Custom archetypes are also supported — use any name configured in .review.toml.
Groups fan out to multiple archetypes at once (defined under _groups in .review.toml).
Use \"all\" to fan out to every configured archetype.

Pipe instructions via stdin. Sessions are persistent — the agents
already have project context from previous interactions.

Examples:
  review init                                              Create a .review.toml
  echo \"review staged changes for auth issues\" | review security   Send to security sessions
  echo \"full review please\" | review all                           Fan out to all archetypes
  echo \"how to handle X?\" | review competitors                     Fan out to a group
  echo \"check for issues\" | review bugs --dry-run                  Preview the prompt";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to persistent AI sessions",
    after_help = AFTER_HELP,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Archetype, group, or "all"
    pub archetype: Option<String>,

    /// Print the assembled prompt instead of sending it
    #[arg(long)]
    pub dry_run: bool,

    /// Prepend grounding prefix and archetype prompt to stdin
    #[arg(long)]
    pub anchor: bool,
}

impl Cli {
    pub fn print_help() {
        let mut cmd = Self::command();
        let _ = cmd.print_help();
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a starter .review.toml in the current directory
    Init,
}
