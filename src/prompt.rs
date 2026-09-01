/// Assemble the message sent to a fresh session: the archetype's priming
/// prompt, then the operator's stdin instructions. Grounding (role, read/write
/// intent, "inspect current state") lives in the archetype prompt itself.
///
/// Special case: a slash-command archetype (e.g. `goal = "/goal "`). The
/// claude/codex harness only treats text on the *same line* as the command
/// as its argument, so the normal blank-line separator would fire `/goal`
/// with an empty argument and orphan the operator's real goal a line below.
/// When the prime is a bare single-line slash command, the stdin is inlined
/// onto that line (`/goal <stdin>`) so the command receives it as its argument.
///
/// Special case: an empty prime (`bare = ""`), whose whole point is that the
/// operator's prompt is the entire instruction. The separator is dropped rather
/// than applied to nothing, which would prepend two newlines to a prompt that
/// advertises itself as carrying no priming. Whitespace-only counts as empty -
/// a prime of `" "` primes exactly as much as `""` does, and the difference is
/// invisible in a config file.
pub fn assemble(prime: &str, stdin_instructions: &str) -> String {
    if prime.trim().is_empty() {
        return stdin_instructions.to_string();
    }
    if is_slash_command(prime) {
        return format!("{} {}", prime.trim_end(), stdin_instructions.trim_start());
    }
    format!("{prime}\n\n{stdin_instructions}")
}

/// A prime that is nothing but a slash command on a single line: `/word`,
/// optionally with trailing whitespace or inline text the archetype itself
/// carries. Multi-line primes fall through to the normal separator.
fn is_slash_command(prime: &str) -> bool {
    let trimmed = prime.trim();
    !trimmed.contains('\n')
        && trimmed.starts_with('/')
        && trimmed.chars().nth(1).is_some_and(char::is_alphanumeric)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn assemble_puts_prime_before_stdin() {
        let out = assemble("you are a bugs expert", "review staged changes");
        let prime_pos = out.find("you are a bugs expert").unwrap();
        let stdin_pos = out.find("review staged changes").unwrap();
        assert!(prime_pos < stdin_pos);
    }

    #[test]
    fn slash_command_inlines_stdin_on_same_line() {
        // `/goal ` + stdin must land on one line so the harness sees the
        // stdin as the command's argument, not an orphaned message below.
        let out = assemble("/goal ", "ship the auth refactor");
        assert_eq!(out, "/goal ship the auth refactor");
    }

    #[test]
    fn slash_command_without_trailing_space_still_inlines() {
        let out = assemble("/goal", "ship it");
        assert_eq!(out, "/goal ship it");
    }

    #[test]
    fn non_slash_prime_keeps_blank_line_separator() {
        let out = assemble("you are a bugs expert", "review it");
        assert_eq!(out, "you are a bugs expert\n\nreview it");
    }

    #[test]
    fn empty_prime_sends_stdin_verbatim() {
        // `bare = ""` must send exactly what the operator typed. The blank-line
        // separator applied to an empty prime prepends two newlines to every
        // bare prompt.
        let out = assemble("", "what is the mechanism here?");
        assert_eq!(out, "what is the mechanism here?");
    }

    #[test]
    fn whitespace_only_prime_is_bare_too() {
        // Indistinguishable from `""` in a config file, so it must behave the
        // same rather than emitting the separator plus the stray whitespace.
        let out = assemble("  \n ", "go");
        assert_eq!(out, "go");
    }

    #[test]
    fn empty_prime_preserves_stdin_leading_whitespace() {
        // The separator is what gets dropped, not the operator's own text: a
        // prompt that deliberately opens with an indented block keeps it.
        let out = assemble("", "  indented first line\nsecond");
        assert_eq!(out, "  indented first line\nsecond");
    }

    #[test]
    fn multiline_prime_starting_with_slash_is_not_treated_as_command() {
        // A real archetype prompt that merely happens to open with `/` and
        // spans lines is not a slash command; keep the separator.
        let prime = "/tmp scanning is off-topic;\nfocus on correctness";
        let out = assemble(prime, "go");
        assert_eq!(out, format!("{prime}\n\ngo"));
    }
}
