/// Assemble the message sent to a fresh session: the archetype's priming
/// prompt, then a blank line, then the operator's stdin instructions. Grounding
/// (role, read/write intent, "inspect current state") lives in the archetype
/// prompt itself.
///
/// Special case: an empty prime - a run with no archetype, or an archetype such
/// as `bare = ""` - whose whole point is that the operator's prompt is the
/// entire instruction. The separator is dropped rather than applied to nothing,
/// which would prepend two newlines to a prompt that advertises itself as
/// carrying no priming. Whitespace-only counts as empty - a prime of `" "`
/// primes exactly as much as `""` does, and the difference is invisible in a
/// config file.
///
/// There is deliberately no slash-command handling. Inlining stdin onto a
/// `/goal ` prime's line was tried; `/goal` works in an interactive session but
/// does nothing when sent headless through `review`, so the special case served
/// nothing.
pub fn assemble(prime: &str, stdin_instructions: &str) -> String {
    if prime.trim().is_empty() {
        return stdin_instructions.to_string();
    }
    format!("{prime}\n\n{stdin_instructions}")
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
    fn prime_is_separated_by_a_blank_line() {
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
    fn slash_prime_gets_no_special_treatment() {
        // Existing configs still carry `goal = "/goal "`; it is an ordinary
        // prime now, separated like any other.
        let out = assemble("/goal ", "ship it");
        assert_eq!(out, "/goal \n\nship it");
    }
}
