use clap::{CommandFactory, Parser, Subcommand};

const PRIME_LONG_ABOUT: &str = "\
Create new provider sessions for an archetype and add them to .review.toml.

The priming prompt is stored in .review.toml under [_prime] on first use,
so if a session later breaks you can re-prime without retyping it:

  First prime (stdin required — prompt gets stored):
    echo \"you are a bugs expert\" | review prime bugs --provider claude

  Re-prime later (stdin omitted — stored prompt is reused, new session created):
    review prime bugs --provider claude

Re-priming replaces stale session IDs in-place. Manually-added `model` and
`env` overrides on a provider entry are preserved.

Re-piping the same prompt is fine (silent reuse) — that's what happens when
you run `echo \"...\" | review prime ARCH --provider X` once per provider in
sequence. Passing stdin that DIFFERS from the stored prompt is an error; in
that case, remove the entry from [_prime] to replace it.

Each successful priming writes a sidecar entry (kind = \"prime\") so
`review sessions` lists primed sessions alongside --oneshot ones.";

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
  echo \"You are a bugs expert\" | review prime bugs --provider claude  Create a session (prompt stored)
  review prime bugs --provider claude                      Re-prime using the stored prompt
  echo \"review staged changes\" | review security                   Send to security sessions
  echo \"full review please\" | review all                           Fan out to all archetypes
  echo \"review please\" | review security,bugs,arch                 Multiple archetypes
  echo \"how to handle X?\" | review competitors                     Fan out to a group
  echo \"re-anchor please\" | review bugs --anchor                   Prepend grounding prefix
  echo \"check now\" | review --oneshot security,bugs               Fresh sessions, prepend stored prime
  echo \"follow up\" | review bugs --provider claude --session ID    Resume a specific session
  echo \"just claude\" | review bugs --provider claude               Only use claude
  echo \"check for issues\" | review bugs --dry-run                  Preview the prompt";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to persistent AI sessions",
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

    /// Prepend grounding prefix to stdin
    #[arg(long)]
    pub anchor: bool,

    /// Skip session resume; start a fresh persistable session and prepend the stored prime prompt.
    /// For claude and codex, the new session ID is printed to stdout (above the response) so it
    /// can be reused via --session for cache-warm follow-ups. Implies --anchor.
    #[arg(long)]
    pub oneshot: bool,

    /// Resume a specific session ID (no prefix, prime, or anchor prepended).
    /// Requires a single --provider; mutually exclusive with --oneshot and --anchor.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Limit to specific providers (comma-separated, e.g. claude,kilo)
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

    /// Create new provider sessions for an archetype and add them to .review.toml
    #[command(long_about = PRIME_LONG_ABOUT)]
    Prime {
        /// Archetype name to create sessions for
        archetype: String,

        /// Providers to create sessions for (comma-separated, e.g. claude,codex)
        #[arg(long, value_delimiter = ',', required = true)]
        provider: Vec<String>,
    },

    /// List recent --oneshot sessions for follow-up via --session
    Sessions {
        /// List sessions across all projects, not just the current one
        #[arg(long)]
        all: bool,

        /// Maximum number of sessions to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },
}
