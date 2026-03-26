const PREFIX: &str = include_str!("../prompts/prefix.md");

pub fn assemble(stdin_instructions: &str) -> String {
    format!("{PREFIX}\n\n{stdin_instructions}")
}
