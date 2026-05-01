const PREFIX: &str = include_str!("../prompts/prefix.md");

pub fn assemble(stdin_instructions: &str) -> String {
    format!("{PREFIX}\n\n{stdin_instructions}")
}

pub fn assemble_oneshot(prime: Option<&str>, stdin_instructions: &str) -> String {
    match prime {
        Some(p) => format!("{PREFIX}\n\n{p}\n\n{stdin_instructions}"),
        None => format!("{PREFIX}\n\n{stdin_instructions}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn oneshot_with_prime_includes_all_three_pieces() {
        let out = assemble_oneshot(Some("you are a bugs expert"), "review staged changes");
        assert!(out.contains(PREFIX));
        assert!(out.contains("you are a bugs expert"));
        assert!(out.contains("review staged changes"));
        let prime_pos = out.find("you are a bugs expert").unwrap();
        let stdin_pos = out.find("review staged changes").unwrap();
        assert!(prime_pos < stdin_pos);
    }

    #[test]
    fn oneshot_without_prime_skips_prime_block() {
        let out = assemble_oneshot(None, "review staged changes");
        assert_eq!(out, format!("{PREFIX}\n\nreview staged changes"));
    }
}
