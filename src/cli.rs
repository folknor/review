use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Instructions come on stdin. With no archetype they are sent unchanged; with
-a, the archetype's priming prompt is prepended. Every run starts a fresh
session on each provider, and the session ID is printed above the response so
you can follow up with `review resume <ID>` while the cache is warm.

Settings resolve from the command line, then the project's .review.toml, then
the global config ($XDG_CONFIG_HOME/review/config.toml, else
~/.config/review/config.toml). `review config` shows the effective result and
where each value came from.

Providers: claude, codex, grok, from --provider or [_defaults].providers. A
provider that is not installed fails the run before anything launches.

Profiles (-p): a profile resolves to its first definition, looking in the
project file before the global one, and that definition wins whole. Fields are
never merged, so a project profile that leaves out `model` does not inherit the
global profile's `model`. `sandbox` defaults to read-only, so a profile has to
ask for workspace-write before a run may modify files. Claude ignores `sandbox`:
what a claude run can touch is decided by Claude Code's own permission settings.

Output: each provider prints a `--- <provider> ---` block, then `session: <ID>`,
then the response. Codex and grok runs also print a digest between the session
line and the response:
  exit / signal     the provider process's exit status
  captured          true if a final answer was captured; false means the text
                    below is an interim note, not a conclusion
  turn failed       the reason the provider gave for ending the turn
  terminated by review
                    the stall watchdog killed a codex run that went silent
  recovered         the final answer was restored from codex's transcript
  turns / usage     turn count and token usage (input, cached, output, reasoning)
  transcript        codex's rollout file and its last events
  incident          a forensic bundle for the run (see `review incidents`)
The output ends with a `runtime:` line. The exit status is 1 when no provider
produced an answer, including runs that died, stalled or were interrupted.

Examples:
  echo \"what does foo() do?\" | review -p deep          Plain prompt, 'deep' profile
  echo \"audit the auth flow\" | review -a security      With an archetype
  echo \"full sweep\" | review -a security,bugs          Several archetypes
  echo \"how to handle X?\" | review -a competitors      A group of archetypes
  echo \"everything\" | review -a all                    Every configured archetype
  echo \"just claude\" | review --provider claude        Only one provider
  echo \"check\" | review -a bugs --dry-run              Preview the prompt
  echo \"follow up\" | review resume <ID>                Continue a session
  review interrupt <ID>                                Stop a codex run mid-turn
  review config                                        Effective configuration";

// The cutoffs are restated from `timings::stale_session` because clap needs a
// `&'static str`; a test keeps the two in step.
const RESUME_AFTER_HELP: &str = "\
The session must come from a run on this host: its provider is read from the
record that run left (`review sessions` lists them), and an ID with no record
is refused. There is no --provider, -a or -p. Stdin is sent as-is, with no
archetype prime, and the session keeps the sandbox, model and effort it was
started with.

Resuming only pays off while the provider's prompt cache is warm, so a session
last touched more than 27 minutes ago (codex) or 55 minutes ago (claude, grok)
is refused. When that happens, start a fresh run and restate the context the
new session needs.

The exit status is 1 if the turn produced no answer.";

const INTERRUPT_AFTER_HELP: &str = "\
Only works on codex runs, which cannot be sent a message mid-turn. This ends
the turn, waits for the run to record its session, and prints the
`review resume` command. The interrupted run exits 1, like any run that
produced no answer.";

const CONFIG_AFTER_HELP: &str = "\
This is the authoritative view of what a run in this directory will use. It
merges the project's .review.toml with the global config, so reading
.review.toml alone gives an incomplete and often wrong answer: profiles and
archetypes may live in the global file, and legacy host tables may shadow them.
Lists the files consulted, the default providers and whether each is installed
on this host, archetypes, groups, and profiles, each with its source and any
definition it shadows. Env vars are listed by name only, never by value.
Outside a project it shows the global layer alone.";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Send a prompt to fresh AI sessions across providers",
    override_usage = "echo <instructions> | review [OPTIONS]\n       review <COMMAND>",
    after_help = AFTER_HELP,
    subcommand_precedence_over_arg = true,
    // The run flags belong to the run. Without this clap accepts them in front
    // of a subcommand and the subcommand never sees them: `review --dry-run
    // resume <ID>` parsed, dropped the `--dry-run`, and sent the turn for real.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Archetype(s) to prime with: a name, a group, a comma-separated list, or
    /// "all". Omit to send stdin unchanged.
    #[arg(short = 'a', long, value_name = "NAME")]
    pub archetype: Option<String>,

    /// Named profile (model/effort/sandbox/env), resolved per launched provider
    /// from [<provider>.<profile>]. The first definition found wins whole;
    /// sandbox defaults to read-only and claude ignores it (see below)
    #[arg(short = 'p', long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Limit to specific providers (comma-separated, e.g. claude,codex)
    #[arg(long, value_delimiter = ',')]
    pub provider: Option<Vec<String>>,

    /// Print the assembled prompt instead of sending it
    #[arg(long)]
    pub dry_run: bool,

    /// Seconds between provider launches, to avoid rate limits (0 disables)
    // Default sourced from `timings` so every production timing value has one
    // home; clap needs a `&'static str`, hence the const rather than the literal.
    #[arg(long, default_value = crate::timings::STAGGER_SECS_STR)]
    pub stagger: u64,

    /// Migration: the archetype used to be positional (`review security`). Still
    /// accepted, with a warning, so orchestration procedures written against
    /// the old form keep working until they are updated; `bare` means none.
    #[arg(hide = true, value_name = "ARCHETYPE")]
    pub legacy_archetype: Option<String>,

    /// Migration: `--session <ID>` is the old spelling of `review resume <ID>`.
    #[arg(long, hide = true, value_name = "ID")]
    pub session: Option<String>,
}

impl Cli {
    pub fn print_help() {
        let mut cmd = Self::command();
        let _ = cmd.print_help();
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Continue a session from an earlier run, sending stdin as the next turn
    #[command(after_help = RESUME_AFTER_HELP)]
    Resume {
        /// Session ID, as printed above the earlier run's response
        #[arg(value_name = "ID")]
        id: String,

        /// Print what would be sent instead of sending it
        #[arg(long)]
        dry_run: bool,
    },

    /// Interrupt a codex run in flight, then print how to resume its session
    #[command(after_help = INTERRUPT_AFTER_HELP)]
    Interrupt {
        /// Session ID of the run, as listed by `review sessions`
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Show the effective configuration and where each value came from
    #[command(after_help = CONFIG_AFTER_HELP)]
    Config,

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

    /// List recent forensic bundles written for suspicious codex runs
    Incidents {
        /// Maximum number of incidents to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Create a starter .review.toml in the current directory
    Init,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("review").chain(args.iter().copied()))
    }

    fn parsed(args: &[&str]) -> Cli {
        match parse(args) {
            Ok(cli) => cli,
            Err(e) => panic!("{args:?} should parse: {e}"),
        }
    }

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn resume_help_states_the_current_stale_cutoffs() {
        for (provider, label) in [("codex", "(codex)"), ("claude", "(claude, grok)")] {
            let mins = crate::timings::stale_session(provider).as_secs() / 60;
            let needle = format!("{mins} minutes ago {label}");
            assert!(
                RESUME_AFTER_HELP.replace('\n', " ").contains(&needle),
                "resume help should say {needle:?}"
            );
        }
        assert_eq!(
            crate::timings::stale_session("grok"),
            crate::timings::stale_session("claude")
        );
    }

    #[test]
    fn a_run_takes_its_archetype_and_profile_as_flags() {
        let cli = parsed(&["-a", "security,bugs", "-p", "deep", "--dry-run"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.archetype.as_deref(), Some("security,bugs"));
        assert_eq!(cli.profile.as_deref(), Some("deep"));
        assert!(cli.dry_run);
        assert!(cli.legacy_archetype.is_none());
    }

    #[test]
    fn a_plain_run_needs_no_arguments_at_all() {
        let cli = parsed(&[]);
        assert!(cli.command.is_none());
        assert!(cli.archetype.is_none());
        assert!(cli.legacy_archetype.is_none());
    }

    #[test]
    fn resume_takes_its_own_dry_run() {
        match parsed(&["resume", "abc", "--dry-run"]).command {
            Some(Command::Resume { id, dry_run }) => {
                assert_eq!(id, "abc");
                assert!(dry_run);
            }
            _ => panic!("expected resume"),
        }
    }

    #[test]
    fn run_flags_in_front_of_a_subcommand_are_refused() {
        // Accepting them meant dropping them: `--dry-run resume <ID>` parsed,
        // the subcommand never saw the flag, and a real turn was sent.
        for args in [
            &["--dry-run", "resume", "abc"][..],
            &["-p", "deep", "resume", "abc"],
            &["-a", "bugs", "resume", "abc"],
            &["--provider", "claude", "resume", "abc"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should be refused");
        }
    }

    #[test]
    fn a_run_flag_never_reaches_a_subcommand() {
        // After a run flag clap stops matching subcommands, so a lone word is
        // taken as the legacy positional archetype instead: `-p deep config`
        // is a run primed with an archetype named `config` (warned about, and
        // an error unless one exists) - never `review config` with the flag
        // silently dropped.
        let cli = parsed(&["-p", "deep", "config"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.legacy_archetype.as_deref(), Some("config"));
    }

    #[test]
    fn the_old_positional_archetype_still_parses() {
        let cli = parsed(&["security", "--profile", "deep"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.legacy_archetype.as_deref(), Some("security"));
        assert_eq!(cli.profile.as_deref(), Some("deep"));
    }

    #[test]
    fn the_old_session_flag_still_parses() {
        let cli = parsed(&["bugs", "--session", "abc", "--provider", "codex"]);
        assert_eq!(cli.session.as_deref(), Some("abc"));
    }

    #[test]
    fn interrupt_takes_a_session_id() {
        match parsed(&["interrupt", "abc"]).command {
            Some(Command::Interrupt { id }) => assert_eq!(id, "abc"),
            _ => panic!("expected interrupt"),
        }
        assert!(parse(&["interrupt"]).is_err(), "the id is required");
    }

    #[test]
    fn a_subcommand_name_is_the_subcommand() {
        assert!(matches!(parsed(&["config"]).command, Some(Command::Config)));
        assert!(matches!(
            parsed(&["sessions", "--all"]).command,
            Some(Command::Sessions { all: true, .. })
        ));
    }
}
