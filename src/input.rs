use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};

const MAX_STDIN_BYTES: usize = 20_000;

pub fn read_stdin() -> Result<String> {
    if std::io::stdin().is_terminal() {
        bail!(
            "no instructions provided on stdin\n\n\
             Pipe your instructions via stdin, e.g.:\n  \
             echo \"review for security issues\" | review security"
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
