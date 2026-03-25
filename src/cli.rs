use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
Quick start:
  review init                                                Create a .review.md
  echo \"check for auth issues\" | review security --staged    Run a review
  echo \"full review\" | review all --staged                   Review with all archetypes
  echo \"check logging\" | review logging --general            Custom archetype";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to persistent AI sessions",
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Archetype name (e.g. security, bugs, perf, arch, or custom) or "all"
    #[arg(required_unless_present = "command")]
    pub archetype: Option<String>,

    #[command(flatten)]
    pub input: InputSource,
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
