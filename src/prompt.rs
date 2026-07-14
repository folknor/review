/// Assemble the message sent to a fresh session: the archetype's priming
/// prompt, then the operator's stdin instructions. Grounding (role, read/write
/// intent, "inspect current state") lives in the archetype prompt itself.
pub fn assemble(prime: &str, stdin_instructions: &str) -> String {
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
}
