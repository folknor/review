use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};

pub const MAX_STDIN_BYTES: usize = 20_000;

pub fn read_stdin() -> Result<String> {
    match read_stdin_optional()? {
        Some(s) => Ok(s),
        None => bail!(
            "no instructions provided on stdin\n\n\
             Pipe your instructions via stdin, e.g.:\n  \
             echo \"review for security issues\" | review security"
        ),
    }
}

/// Read stdin if piped; return None if stdin is a terminal or empty.
///
/// `is_terminal()` alone is not enough: when invoked from a non-interactive
/// parent (Claude Code's Bash tool, scripts, CI, xargs), stdin is not a TTY
/// but also has no data. Treat an empty read as "no input."
pub fn read_stdin_optional() -> Result<Option<String>> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    std::io::stdin()
        .take(MAX_STDIN_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .context("failed to read from stdin")?;
    if buf.len() > MAX_STDIN_BYTES {
        bail!("stdin instructions exceed {MAX_STDIN_BYTES} byte limit");
    }
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(buf))
}
