use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};

use crate::cli::InputSource;

const MAX_STDIN_BYTES: usize = 20_000;

pub fn context_line(input: &InputSource) -> String {
    if input.unstaged {
        return "You are reviewing unstaged changes.".into();
    }
    if input.staged {
        return "You are reviewing staged changes.".into();
    }
    if let Some(ref hash) = input.commit {
        return format!("You are reviewing commit {hash}.");
    }
    if let Some(ref range) = input.range {
        return format!("You are reviewing commits {range}.");
    }
    if let Some(ref path) = input.document {
        return format!("You are reviewing the file {path}.");
    }
    if input.general {
        return "You are reviewing the entire codebase.".into();
    }
    unreachable!()
}

pub fn read_stdin() -> Result<String> {
    if std::io::stdin().is_terminal() {
        bail!(
            "no instructions provided on stdin\n\n\
             Pipe your review instructions via stdin, e.g.:\n  \
             echo \"review for security issues\" | review security --staged"
        );
    }
    let mut buf = String::new();
    std::io::stdin()
        .take(MAX_STDIN_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .context("failed to read from stdin")?;
    if buf.len() > MAX_STDIN_BYTES {
        bail!("stdin instructions exceed {MAX_STDIN_BYTES} byte limit");
    }
    Ok(buf)
}
