use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Archetypes are named reviewer personas defined under [archetypes] in
.review.toml (name = priming prompt). Groups fan out to multiple archetypes
(defined under [_groups]). Use \"all\" to fan out to every configured archetype.

Providers: claude, codex. Providers come from --provider, or
[_defaults].providers when --provider is omitted.

Each run starts a fresh session, prepends the archetype's priming prompt, and
lets the agent fetch code itself. For claude and codex the new session ID is
printed above the response so you can follow up while the cache is warm via
--session.

Examples:
  review init                                              Create a .review.toml
  echo \"review staged changes\" | review security                   Send to a security session
  echo \"full review please\" | review all                           Fan out to all archetypes
  echo \"review please\" | review security,bugs,arch                 Multiple archetypes
  echo \"how to handle X?\" | review competitors                     Fan out to a group
  echo \"check now\" | review security --profile opus               Apply the 'opus' profile
  echo \"follow up\" | review bugs --provider claude --session ID    Resume a specific session
  echo \"just claude\" | review bugs --provider claude               Only use claude
  echo \"check for issues\" | review bugs --dry-run                  Preview the prompt";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to fresh AI sessions",
    override_usage = "review [ARCHETYPE|COMMAND] [OPTIONS]",
    after_help = AFTER_HELP,
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Archetype, group, or "all"
    #[arg(help_heading = "Archetype")]
    pub archetype: Option<String>,

    /// Print the assembled prompt instead of sending it
    #[arg(long)]
    pub dry_run: bool,

    /// Apply a named profile's model/effort/env overrides. Resolved per launched
    /// provider from [<host>.<provider>.<profile>] in .review.toml.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Resume a specific session ID (no prime prepended).
    /// Requires a single --provider.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Limit to specific providers (comma-separated, e.g. claude,codex)
    #[arg(long, value_delimiter = ',')]
    pub provider: Option<Vec<String>>,

    /// Seconds between each provider launch to avoid rate limits (default: 30, 0 to disable)
    #[arg(long, default_value = "30")]
    pub stagger: u64,
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

    /// List recent sessions for follow-up via --session
    Sessions {
        /// List sessions across all projects, not just the current one
        #[arg(long)]
        all: bool,

        /// Maximum number of sessions to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },
}
