use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};

const MAX_STDIN_BYTES: usize = 20_000;

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

/// Read stdin if piped; return None if stdin is a terminal.
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
    Ok(Some(buf))
}
