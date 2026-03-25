use clap::Parser;

use crate::config::BUILTIN_ARCHETYPES;

fn archetype_help() -> String {
    format!(
        "Archetype name or \"all\". Built-in: {}",
        BUILTIN_ARCHETYPES.join(", ")
    )
}

#[derive(Parser)]
#[command(name = "review", about = "Fan out code reviews to persistent AI sessions")]
pub struct Cli {
    /// Archetype to review with
    #[arg(help = archetype_help())]
    pub archetype: String,

    #[command(flatten)]
    pub input: InputSource,
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
}

impl InputSource {
    pub fn is_specified(&self) -> bool {
        self.unstaged
            || self.staged
            || self.commit.is_some()
            || self.range.is_some()
            || self.document.is_some()
    }
}
