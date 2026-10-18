//! `review man [TOPIC]` - the bundled topics behind `--help`.
//!
//! `--help` and `review config` are meant to be enough to drive `review`; a
//! topic is for when an agent needs the rule behind what it saw. Each is one
//! `docs/*.md` file compiled in with `include_str!` and printed raw: markdown
//! is what an agent reads best, and `review` prints plain text everywhere.
//!
//! A topic is printed whole. There is no section addressing - each topic is
//! capped in size instead (`tests::MAX_TOPIC_BYTES`), so one that grows is split into
//! another topic, and every topic has to be safe to act on alone: a rule and
//! its exception live in the same file.
//!
//! The docs describe observable behaviour. They never link into `reference/`
//! or by repository path - an installed binary has no checkout to follow the
//! link into - and refer to each other only as complete `review man <topic>`
//! commands, which the tests below resolve.

use std::io::Write;

struct Topic {
    name: &'static str,
    summary: &'static str,
    /// Relative to the repository root. Read only by the catalog test, which
    /// checks it against both `docs/` and what `include_str!` compiled in.
    #[cfg_attr(not(test), allow(dead_code))]
    path: &'static str,
    content: &'static str,
}

/// Every topic, in listing order. `include_str!` paths are relative to `src/`.
const TOPICS: &[Topic] = &[
    Topic {
        name: "run",
        summary: "a fresh run: archetypes, fan-out, dry runs, auto-resume, exit status",
        path: "docs/run.md",
        content: include_str!("../docs/run.md"),
    },
    Topic {
        name: "config",
        summary: "the config files, how they layer, every section, reserved words",
        path: "docs/config.md",
        content: include_str!("../docs/config.md"),
    },
    Topic {
        name: "profiles",
        summary: "[<provider>.<profile>] and -p: every field, whole-profile precedence",
        path: "docs/profiles.md",
        content: include_str!("../docs/profiles.md"),
    },
    Topic {
        name: "sandbox",
        summary: "what each sandbox level lets a run touch, per provider; writable roots",
        path: "docs/sandbox.md",
        content: include_str!("../docs/sandbox.md"),
    },
    Topic {
        name: "message",
        summary: "steering a session: interrupt, resume, stale cutoffs, queued messages",
        path: "docs/message.md",
        content: include_str!("../docs/message.md"),
    },
    Topic {
        name: "failures",
        summary: "why a run exited 1: death, stall, stated failure, interrupt",
        path: "docs/failures.md",
        content: include_str!("../docs/failures.md"),
    },
    Topic {
        name: "sessions",
        summary: "review sessions and review incidents: reading what a run left behind",
        path: "docs/sessions.md",
        content: include_str!("../docs/sessions.md"),
    },
    Topic {
        name: "projects",
        summary: "-C: acting on another project, the [_projects] registry",
        path: "docs/projects.md",
        content: include_str!("../docs/projects.md"),
    },
];

/// Print `topic`, or the index with none. Returns the exit status.
///
/// An unknown topic is an error on stderr, index included, with nothing on
/// stdout: a caller capturing stdout must not mistake the index for the
/// document it asked for.
pub fn run(topic: Option<&str>) -> i32 {
    let Some(name) = topic else {
        return write_stdout(&index());
    };
    match TOPICS.iter().find(|t| t.name == name) {
        Some(t) => write_stdout(t.content),
        None => {
            eprintln!("error: no man topic '{name}'\n\n{}", index());
            1
        }
    }
}

fn index() -> String {
    let width = TOPICS.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let mut out = String::from("Topics. Run `review man <topic>` to read one.\n\n");
    for t in TOPICS {
        out.push_str(&format!("  {:width$}  {}\n", t.name, t.summary));
    }
    out
}

/// A closed pipe (`| head`) is a clean exit; any other write failure fails the
/// command, since the reader did not get the document.
fn write_stdout(text: &str) -> i32 {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            eprintln!("error: writing the topic failed: {e}");
            1
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The most a topic may hold. Past it, split the topic rather than grow
    /// it: a topic is printed whole, so its size is what a reader pays.
    const MAX_TOPIC_BYTES: usize = 12_000;

    fn topic(name: &str) -> &'static str {
        TOPICS.iter().find(|t| t.name == name).unwrap().content
    }

    #[test]
    fn names_and_paths_are_unique_and_match_the_docs_folder() {
        let names: BTreeSet<&str> = TOPICS.iter().map(|t| t.name).collect();
        let paths: BTreeSet<&str> = TOPICS.iter().map(|t| t.path).collect();
        assert_eq!(names.len(), TOPICS.len(), "duplicate topic name");
        assert_eq!(paths.len(), TOPICS.len(), "two topics share a file");

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut on_disk = BTreeSet::new();
        for entry in std::fs::read_dir(root.join("docs")).unwrap() {
            let path = entry.unwrap().path();
            assert!(!path.is_dir(), "docs/ is flat: {}", path.display());
            if path.extension().is_some_and(|e| e == "md") {
                let rel = path.strip_prefix(root).unwrap().to_string_lossy();
                on_disk.insert(rel.into_owned());
            }
        }
        let catalog: BTreeSet<String> = paths.iter().map(|p| (*p).to_string()).collect();
        assert_eq!(on_disk, catalog, "docs/*.md and the topic catalog differ");

        for t in TOPICS {
            let file = std::fs::read_to_string(root.join(t.path)).unwrap();
            assert_eq!(
                file, t.content,
                "{} is not what {} includes",
                t.path, t.name
            );
        }
    }

    #[test]
    fn every_topic_is_nonempty_and_within_the_size_cap() {
        for t in TOPICS {
            assert!(!t.content.trim().is_empty(), "{} is empty", t.name);
            assert!(
                t.content.len() <= MAX_TOPIC_BYTES,
                "{} is {} bytes, over {MAX_TOPIC_BYTES}: split it into another topic",
                t.name,
                t.content.len()
            );
        }
    }

    /// A cross-reference is a complete command - `review man` or `review man
    /// <topic>` inside backticks, the one spelling the docs use - and every one
    /// must resolve. Anything else around `review man` is an error, opening
    /// backtick included, so a renamed topic cannot linger in a reference that
    /// no longer works.
    #[test]
    fn every_man_reference_names_a_topic() {
        for t in TOPICS {
            for (i, _) in t.content.match_indices("review man") {
                assert!(
                    t.content[..i].ends_with('`'),
                    "{}: `review man` reference not opened by a backtick",
                    t.name
                );
                let rest = &t.content[i + "review man".len()..];
                if rest.starts_with('`') {
                    continue;
                }
                let target = rest
                    .strip_prefix(' ')
                    .and_then(|r| r.split_once('`'))
                    .map(|(name, _)| name);
                match target {
                    Some(name) if TOPICS.iter().any(|x| x.name == name) => {}
                    _ => panic!(
                        "{}: `review man{}` is not `review man` or `review man <topic>` in backticks",
                        t.name,
                        rest.lines().next().unwrap_or("")
                    ),
                }
            }
        }
    }

    /// Navigation is by command only: a repository path means nothing to a
    /// reader holding only the binary. This catches inline links, reference
    /// definitions, autolinks, HTML anchors and any mention of a `.md` file or
    /// the reference folder. A bare `[shortcut]` reference cannot be told
    /// apart from a TOML table name like `[_audit]`, so it is the one form not
    /// caught; a shortcut without a definition is not a link anyway.
    #[test]
    fn docs_carry_no_links() {
        for t in TOPICS {
            let lower = t.content.to_ascii_lowercase();
            for bad in [
                "](",
                "<a ",
                "<a\n",
                "<a\t",
                "href",
                "<http",
                "reference/",
                ".md",
            ] {
                assert!(!lower.contains(bad), "{} contains {bad:?}", t.name);
            }
            // Any `<scheme:...>` autolink, `mailto:` included, and the bare
            // `<user@host>` email form.
            for (i, _) in t.content.match_indices('<') {
                let rest = &t.content[i + 1..];
                if let Some((inner, _)) = rest.split_once('>') {
                    assert!(
                        !(inner.contains('@') && !inner.contains(char::is_whitespace)),
                        "{}: email autolink <{inner}>",
                        t.name
                    );
                }
                let scheme: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || "+.-".contains(*c))
                    .collect();
                assert!(
                    scheme.is_empty() || !rest[scheme.len()..].starts_with(':'),
                    "{}: autolink <{scheme}:...>",
                    t.name
                );
            }
            // A reference definition, in any container: nothing else in these
            // docs needs the sequence, so it is banned outright rather than
            // parsed for.
            assert!(
                !t.content.contains("]:"),
                "{}: `]:` (a reference definition)",
                t.name
            );
        }
    }

    /// Rows of the first markdown table whose header starts with `header`.
    fn table(doc: &str, header: &str) -> Vec<Vec<String>> {
        let mut lines = doc.lines().skip_while(|l| !l.starts_with(header));
        assert!(lines.next().is_some(), "no table headed {header:?}");
        lines
            .skip(1) // the |---| rule
            .take_while(|l| l.starts_with('|'))
            .map(|l| {
                l.trim_matches('|')
                    .split('|')
                    .map(|c| c.trim().trim_matches('`').to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_stale_cutoff_table_states_every_providers_cutoff() {
        let rows = table(
            topic("message"),
            "| Provider | Refused when last touched more than |",
        );
        let documented: BTreeSet<(String, String)> = rows
            .into_iter()
            .map(|r| (r[0].clone(), r[1].clone()))
            .collect();
        let actual: BTreeSet<(String, String)> = crate::config::KNOWN_PROVIDERS
            .iter()
            .map(|p| {
                let secs = crate::timings::stale_session(p).as_secs();
                assert_eq!(
                    secs % 60,
                    0,
                    "{p}'s cutoff is not whole minutes; reword the table"
                );
                ((*p).to_string(), format!("{} minutes ago", secs / 60))
            })
            .collect();
        assert_eq!(documented, actual);
    }

    /// Pins the translation each provider receives and that claude records no
    /// level. It does not prove the provider enforces the level - that is the
    /// provider tests' and diagnostics' job - only that the table says what
    /// `review` passes on.
    #[test]
    fn the_sandbox_table_states_each_providers_level() {
        let rows = table(topic("sandbox"), "| `sandbox` | codex | grok | claude |");
        let levels: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            levels,
            ["read-only", "workspace-write", "danger-full-access"]
        );
        for r in &rows {
            assert_eq!(r[1], crate::config::sandbox_for("codex", &r[0]), "{r:?}");
            assert_eq!(r[2], crate::config::sandbox_for("grok", &r[0]), "{r:?}");
            let claude = crate::provider::effective_sandbox("claude", Some(&r[0]));
            assert_eq!(r[3] == "ignored", claude.is_none(), "{r:?} vs {claude:?}");
        }
    }

    #[test]
    fn the_reserved_words_table_is_reserved_names_exactly() {
        let rows = table(topic("config"), "| Reserved word | Why |");
        let documented: BTreeSet<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        let actual: BTreeSet<&str> = crate::config::RESERVED_NAMES.iter().copied().collect();
        assert_eq!(documented, actual);
    }
}
