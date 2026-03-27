use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Archetypes are named reviewer sessions defined in .review.toml.
Groups fan out to multiple archetypes (defined under [_groups]).
Use \"all\" to fan out to every configured archetype.

Providers: claude, codex, kilo, opencode. Use --provider to limit
which providers are used (e.g. --provider claude,kilo).

Pipe instructions via stdin. Sessions are persistent — the agents
already have project context from previous interactions.

Examples:
  review init                                              Create a .review.toml
  echo \"review staged changes\" | review security                   Send to security sessions
  echo \"full review please\" | review all                           Fan out to all archetypes
  echo \"how to handle X?\" | review competitors                     Fan out to a group
  echo \"re-anchor please\" | review bugs --anchor                   Prepend grounding prefix
  echo \"just claude\" | review bugs --provider claude               Only use claude
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

    /// Prepend grounding prefix to stdin
    #[arg(long)]
    pub anchor: bool,

    /// Limit to specific providers (comma-separated, e.g. claude,kilo)
    #[arg(long, value_delimiter = ',')]
    pub provider: Option<Vec<String>>,
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
