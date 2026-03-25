use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
Quick start:
  review init                                                Create a .review.md
  echo \"check for auth issues\" | review security --staged    Run a review
  echo \"full review\" | review all --staged                   Review with all archetypes";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to persistent AI sessions",
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Create a starter .review.md in the current directory
    Init,

    /// Review with the security archetype
    Security {
        #[command(flatten)]
        input: InputSource,
    },

    /// Review with the bugs archetype
    Bugs {
        #[command(flatten)]
        input: InputSource,
    },

    /// Review with the perf archetype
    Perf {
        #[command(flatten)]
        input: InputSource,
    },

    /// Review with the arch archetype
    Arch {
        #[command(flatten)]
        input: InputSource,
    },

    /// Review with all archetypes
    All {
        #[command(flatten)]
        input: InputSource,
    },
}

impl Command {
    pub fn archetype_name(&self) -> &'static str {
        match self {
            Self::Security { .. } => "security",
            Self::Bugs { .. } => "bugs",
            Self::Perf { .. } => "perf",
            Self::Arch { .. } => "arch",
            Self::All { .. } => "all",
            Self::Init => unreachable!(),
        }
    }

    pub fn input_source(&self) -> Option<&InputSource> {
        match self {
            Self::Security { input }
            | Self::Bugs { input }
            | Self::Perf { input }
            | Self::Arch { input }
            | Self::All { input } => Some(input),
            Self::Init => None,
        }
    }
}

#[derive(clap::Args)]
#[group(required = true, multiple = false)]
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
