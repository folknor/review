const DEFAULT_PREFIX: &str = include_str!("../prompts/prefix.md");
const DEFAULT_PROMPT: &str = include_str!("../prompts/default.md");
const SECURITY_PROMPT: &str = include_str!("../prompts/security.md");
const BUGS_PROMPT: &str = include_str!("../prompts/bugs.md");
const PERF_PROMPT: &str = include_str!("../prompts/perf.md");
const ARCH_PROMPT: &str = include_str!("../prompts/arch.md");

fn builtin_prompt(archetype_name: &str) -> &'static str {
    match archetype_name {
        "security" => SECURITY_PROMPT,
        "bugs" => BUGS_PROMPT,
        "perf" => PERF_PROMPT,
        "arch" => ARCH_PROMPT,
        _ => DEFAULT_PROMPT,
    }
}

pub fn assemble(
    archetype_name: &str,
    context: &str,
    stdin_instructions: &str,
) -> String {
    let prefix = DEFAULT_PREFIX;
    let archetype_prompt = builtin_prompt(archetype_name);

    format!("{prefix}\n\n{archetype_prompt}\n\n{stdin_instructions}\n\n{context}")
}
