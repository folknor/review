use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP: &str = "\
Built-in archetypes (with tailored prompts):
  security    Auth boundaries, injection, secrets, trust assumptions
  bugs        Logic errors, edge cases, error handling, crashes
  perf        Allocations, complexity, hot paths, async blocking
  arch        Coupling, abstractions, API design, consistency

Custom archetypes are also supported — use any name configured in .review.md.
Groups fan out to multiple archetypes at once (defined under _groups in .review.md).
Use \"all\" to fan out to every configured archetype.

Instructions are piped via stdin. The agents fetch code themselves —
flags just tell them what to look at.

Examples:
  review init                                                Create a .review.md
  echo \"check for auth issues\" | review security --staged    Security review of staged changes
  echo \"full review\" | review all --general                  All archetypes, entire codebase
  echo \"check logging\" | review logging --general            Custom archetype
  echo \"how to handle X?\" | review competitors --general     Fan out to a group";

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

    /// Send only the piped stdin — no prefix, archetype prompt, or context line
    #[arg(long)]
    pub raw: bool,

    #[command(flatten)]
    pub input: InputSource,
}

impl Cli {
    pub fn print_help() {
        let mut cmd = Self::command();
        let _ = cmd.print_help();
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a starter .review.md in the current directory
    Init,
}

#[derive(clap::Args)]
#[group(required = false, multiple = false)]
pub struct InputSource {
    /// Review unstaged changes
    #[arg(long)]
    pub unstaged: bool,

    /// Review staged changes
    #[arg(long)]
    pub staged: bool,

    /// Review a specific commit
    #[arg(long)]
    pub commit: Option<String>,

    /// Review a commit range (e.g. abc..def)
    #[arg(long)]
    pub range: Option<String>,

    /// Review a file
    #[arg(long)]
    pub document: Option<String>,

    /// Review the entire codebase
    #[arg(long)]
    pub general: bool,
}

impl InputSource {
    pub fn is_specified(&self) -> bool {
        self.unstaged
            || self.staged
            || self.commit.is_some()
            || self.range.is_some()
            || self.document.is_some()
            || self.general
    }
}
