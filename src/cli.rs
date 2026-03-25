use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "review", about = "Fan out code reviews to persistent AI sessions")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a review with the given archetype
    Review {
        /// Archetype name (e.g. security, bugs, perf, arch) or "all"
        archetype: String,

        #[command(flatten)]
        input: InputSource,
    },

    /// Register a provider session for an archetype
    Register {
        /// Archetype name
        archetype: String,

        /// Claude Code session ID
        #[arg(long)]
        claude: Option<String>,

        /// Codex session ID
        #[arg(long)]
        codex: Option<String>,
    },

    /// Deregister an archetype or a specific provider session
    Deregister {
        /// Archetype name
        archetype: String,

        /// Remove only the Claude session
        #[arg(long)]
        claude: bool,

        /// Remove only the Codex session
        #[arg(long)]
        codex: bool,
    },

    /// List archetypes and sessions
    List {
        /// Show all projects, not just the current one
        #[arg(long)]
        all: bool,
    },
}

#[derive(clap::Args)]
#[group(required = false, multiple = false)]
pub struct InputSource {
    /// Working tree changes (git diff)
    #[arg(long)]
    pub unstaged: bool,

    /// Staged changes (git diff --cached)
    #[arg(long)]
    pub staged: bool,

    /// Diff of a specific commit
    #[arg(long)]
    pub commit: Option<String>,

    /// Diff across a commit range (e.g. abc..def)
    #[arg(long)]
    pub range: Option<String>,

    /// Full branch diff against main
    #[arg(long)]
    pub branch: bool,

    /// A file path to review as-is (not a diff)
    #[arg(long)]
    pub document: Option<String>,
}

impl InputSource {
    pub fn is_specified(&self) -> bool {
        self.unstaged
            || self.staged
            || self.commit.is_some()
            || self.range.is_some()
            || self.branch
            || self.document.is_some()
    }
}
