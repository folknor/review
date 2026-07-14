const PREFIX: &str = include_str!("../prompts/prefix.md");

/// Assemble the message sent to a fresh session: grounding prefix, the
/// archetype's priming prompt, then the operator's stdin instructions.
pub fn assemble(prime: &str, stdin_instructions: &str) -> String {
    format!("{PREFIX}\n\n{prime}\n\n{stdin_instructions}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn assemble_includes_all_three_pieces_in_order() {
        let out = assemble("you are a bugs expert", "review staged changes");
        assert!(out.contains(PREFIX));
        let prime_pos = out.find("you are a bugs expert").unwrap();
        let stdin_pos = out.find("review staged changes").unwrap();
        assert!(prime_pos < stdin_pos);
    }
}
