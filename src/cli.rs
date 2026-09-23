use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Settings resolve from the command line, then the project's .review.toml, then
the global config ($XDG_CONFIG_HOME/review/config.toml, else
~/.config/review/config.toml). Both files share one format. `review config`
prints the effective result and where each value came from.

Archetypes are optional priming prompts defined under [archetypes] (name =
prompt). Without one, stdin is sent unchanged. Groups fan out to multiple
archetypes (defined under [_groups]). Use \"all\" to fan out to every
configured archetype.

Providers: claude, codex, grok. Providers come from --provider, or
[_defaults].providers when --provider is omitted. A provider that is not
installed fails the run before anything launches.

Profiles are [<provider>.<profile>] tables selected with --profile. A project
profile replaces a global one of the same name entirely. Legacy
[<host>.<provider>.<profile>] tables still apply on the host they name, and win
over a hostless table in the same file.

Each run starts a fresh session and lets the agent fetch code itself. For all
three providers the new session ID is printed above the response so you can
follow up while the cache is warm via --session.

Examples:
  review init                                              Create a .review.toml
  review config --json                                     Effective config, for scripts
  echo \"what does foo() do?\" | review --profile deep               No archetype: stdin as-is
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

    /// Archetype, group, or "all". Omit to send stdin unchanged.
    #[arg(help_heading = "Archetype")]
    pub archetype: Option<String>,

    /// Print the assembled prompt instead of sending it
    #[arg(long)]
    pub dry_run: bool,

    /// Apply a named profile's model/effort/sandbox/env overrides. Resolved per
    /// launched provider from [<provider>.<profile>], project config first, then
    /// global.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Resume a specific session ID (no prime prepended). The provider is
    /// inferred from the session record when --provider is omitted.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Limit to specific providers (comma-separated, e.g. claude,codex)
    #[arg(long, value_delimiter = ',')]
    pub provider: Option<Vec<String>>,

    /// Seconds between each provider launch to avoid rate limits (default: 30, 0 to disable)
    // Default sourced from `timings` so every production timing value has one
    // home; clap needs a `&'static str`, hence the const rather than the literal.
    #[arg(long, default_value = crate::timings::STAGGER_SECS_STR)]
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

    /// Show the effective configuration: archetypes, groups, providers (and
    /// whether each is installed), and profiles, each with the file it came from
    Config {
        /// Machine-readable output, for orchestrators
        #[arg(long)]
        json: bool,
    },

    /// List recent sessions, or show one session's artifacts by ID
    Sessions {
        /// Session ID to show artifacts for (transcript, digest, response).
        /// Omit to list recent sessions.
        #[arg(value_name = "ID")]
        id: Option<String>,

        /// List sessions across all projects, not just the current one
        #[arg(long)]
        all: bool,

        /// Maximum number of sessions to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// List recent forensic bundles written for suspicious/dead codex runs
    Incidents {
        /// Maximum number of incidents to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },
}
