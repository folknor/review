use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Read};
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
    // Explicit flags take priority over stdin
    if input.is_specified() {
        return resolve_flags(input);
    }

    // Only fall back to stdin when no flags are given and stdin is piped
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read from stdin")?;
        return Ok(ResolvedInput {
            content: buf,
            content_type: ContentType::Diff,
        });
    }

    bail!(
        "no input source specified\n\n\
         Usage: review <archetype> <input-source>\n\n\
         Input sources:\n  \
         --unstaged          working tree changes\n  \
         --staged            staged changes\n  \
         --commit <hash>     diff of a specific commit\n  \
         --range <a..b>      diff across a commit range\n  \
         --branch            full branch diff against main\n  \
         --document <path>   a file as-is\n  \
         <stdin pipe>        piped input"
    );
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
        let output = git(&["show", "--format=", hash])?;
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
        let output = git(&["diff", "main...HEAD"])?;
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

    unreachable!()
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


