use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};
use std::process::Command;

use crate::cli::InputSource;

pub fn resolve(input: &InputSource) -> Result<String> {
    if !input.is_specified() {
        bail!(
            "no input source specified\n\n\
             Usage: review <archetype> <input-source>\n\n\
             Input sources:\n  \
             --unstaged          working tree changes\n  \
             --staged            staged changes\n  \
             --commit <hash>     diff of a specific commit\n  \
             --range <a..b>      diff across a commit range\n  \
             --branch            full branch diff against main\n  \
             --document <path>   a file as-is"
        );
    }

    resolve_flags(input)
}

const MAX_STDIN_BYTES: usize = 20_000;

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

fn resolve_flags(input: &InputSource) -> Result<String> {
    if input.unstaged {
        return git(&["diff"]);
    }

    if input.staged {
        return git(&["diff", "--cached"]);
    }

    if let Some(ref hash) = input.commit {
        return git(&["show", "--format=", "--no-notes", hash]);
    }

    if let Some(ref range) = input.range {
        return git(&["diff", range]);
    }

    if input.branch {
        let base = detect_default_branch()?;
        return git(&["diff", &format!("{base}...HEAD")]);
    }

    if let Some(ref path) = input.document {
        return std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {path}"));
    }

    unreachable!()
}

fn detect_default_branch() -> Result<String> {
    // Try the remote HEAD symref first
    if let Ok(output) = git(&["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let trimmed = output.trim();
        if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
            return Ok(branch.to_string());
        }
    }
    // Fall back to checking common names
    for candidate in ["main", "master"] {
        if git(&["rev-parse", "--verify", candidate]).is_ok() {
            return Ok(candidate.to_string());
        }
    }
    bail!("could not detect default branch (tried origin/HEAD, main, master)")
}

fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
