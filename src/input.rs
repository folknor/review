use anyhow::{Context, Result, bail};
use std::io::Read;
use std::process::Command;

use crate::cli::InputSource;

pub enum ContentType {
    Diff,
    Document,
}

pub struct ResolvedInput {
    pub content: String,
    pub content_type: ContentType,
}

pub fn resolve(input: &InputSource) -> Result<ResolvedInput> {
    if !input.is_specified() {
        bail!(
            "no input source specified\n\n\
             Usage: review <archetype> <input-source>\n\n\
             Input sources:\n  \
             --unstaged              working tree changes\n  \
             --staged                staged changes\n  \
             --commit <hash>         diff of a specific commit\n  \
             --range <a..b>          diff across a commit range\n  \
             --branch                full branch diff against main\n  \
             --document <path>       a file as-is\n  \
             --stdin                 read from stdin (diff)\n  \
             --stdin --as-document   read from stdin (document)"
        );
    }

    resolve_flags(input)
}

fn resolve_flags(input: &InputSource) -> Result<ResolvedInput> {

    if input.unstaged {
        let output = git(&["diff"])?;
        return Ok(ResolvedInput {
            content: output,
            content_type: ContentType::Diff,
        });
    }

    if input.staged {
        let output = git(&["diff", "--cached"])?;
        return Ok(ResolvedInput {
            content: output,
            content_type: ContentType::Diff,
        });
    }

    if let Some(ref hash) = input.commit {
        let output = git(&["show", "--format=", "--no-notes", hash])?;
        return Ok(ResolvedInput {
            content: output,
            content_type: ContentType::Diff,
        });
    }

    if let Some(ref range) = input.range {
        let output = git(&["diff", range])?;
        return Ok(ResolvedInput {
            content: output,
            content_type: ContentType::Diff,
        });
    }

    if input.branch {
        let base = detect_default_branch()?;
        let output = git(&["diff", &format!("{base}...HEAD")])?;
        return Ok(ResolvedInput {
            content: output,
            content_type: ContentType::Diff,
        });
    }

    if let Some(ref path) = input.document {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
        return Ok(ResolvedInput {
            content,
            content_type: ContentType::Document,
        });
    }

    if input.stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read from stdin")?;
        let content_type = if input.as_document {
            ContentType::Document
        } else {
            ContentType::Diff
        };
        return Ok(ResolvedInput {
            content: buf,
            content_type,
        });
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


