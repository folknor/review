use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
Quick start:
  review init                                                Create a .review.md
  echo \"check for auth issues\" | review security --staged    Run a review
  echo \"full review\" | review all --staged                   Review with all archetypes
  echo \"check logging\" | review --type logging --general     Custom archetype";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Fan out code reviews to persistent AI sessions",
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Custom archetype name (use instead of a subcommand)
    #[arg(long = "type", global = false)]
    pub archetype_type: Option<String>,

    #[command(flatten)]
    pub input: InputSource,
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

impl Cli {
    pub fn archetype_name(&self) -> Option<&str> {
        if let Some(ref cmd) = self.command {
            return match cmd {
                Command::Security { .. } => Some("security"),
                Command::Bugs { .. } => Some("bugs"),
                Command::Perf { .. } => Some("perf"),
                Command::Arch { .. } => Some("arch"),
                Command::All { .. } => Some("all"),
                Command::Init => None,
            };
        }
        self.archetype_type.as_deref()
    }

    pub fn input_source(&self) -> Option<&InputSource> {
        if let Some(ref cmd) = self.command {
            return match cmd {
                Command::Security { input }
                | Command::Bugs { input }
                | Command::Perf { input }
                | Command::Arch { input }
                | Command::All { input } => {
                    // If flags were placed before the subcommand, clap routes them
                    // to the top-level Cli.input. Fall back to that if the subcommand's
                    // input is empty.
                    if input.is_specified() {
                        Some(input)
                    } else {
                        Some(&self.input)
                    }
                }
                Command::Init => None,
            };
        }
        Some(&self.input)
    }

    pub fn is_init(&self) -> bool {
        matches!(self.command, Some(Command::Init))
    }
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
