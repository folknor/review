//! Unit tests of the provider runners' pure parts: result classification for
//! claude and grok, permission flags and announcements. The tests that run a
//! real child process are in `tests/provider_runs.rs`, an integration test,
//! because their keeper is the built `review` binary (see `keeper`).

use super::*;

#[tokio::test]
async fn claude_output_past_the_cap_keeps_its_final_result() {
    // The stream ends in the result: keeping only the head would lose it.
    let mut stream = format!("{CLAUDE_INIT}\n");
    for i in 0..200 {
        stream.push_str(&format!("{{\"type\":\"assistant\",\"n\":{i}}}\n"));
    }
    stream.push_str(CLAUDE_OK);
    stream.push('\n');
    let head = new_shared_buf();
    let tail = new_shared_buf();
    let (first_tx, first_rx) = tokio::sync::watch::channel(false);
    // The keep must exceed the result line, as production's 8 MiB does; the
    // stream is several times larger, so the tail really is trimmed.
    let keep = 1024;
    assert!(CLAUDE_OK.len() < keep && stream.len() > 4 * keep);
    read_claude_stream(
        stream.as_bytes(),
        100,
        keep,
        std::sync::Arc::clone(&head),
        std::sync::Arc::clone(&tail),
        first_tx,
    )
    .await;
    assert!(*first_rx.borrow(), "the init event is seen");
    let head = head.lock().expect("head").clone();
    let tail = tail.lock().expect("tail").clone();
    assert_eq!(head.len(), 100, "the head is capped");
    assert!(
        tail.len() <= 2 * keep,
        "the tail is bounded: {}",
        tail.len()
    );
    let joined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(&tail)
    );
    let outcome = super::interpret_claude_output(&joined, "").expect("the result survived");
    assert!(matches!(outcome, super::ClaudeOutcome::Answered { .. }));
}

#[test]
fn a_missing_claude_result_does_not_dump_the_stream_into_the_error() {
    let big = "x".repeat(100_000);
    let stdout = claude_stream(&[
        CLAUDE_INIT,
        &format!(r#"{{"type":"user","tool_result":"{big}"}}"#),
    ]);
    let err = super::interpret_claude_output(&stdout, "").expect_err("no result");
    assert!(
        err.to_string().len() < CLAUDE_DETAIL_MAX + 200,
        "error is {} chars",
        err.to_string().len()
    );
}

// ---------------------------------------------------------------------------
// grok output classification
//
// These need no child process: grok reports the outcome of a turn in its own
// result object, so the whole decision is a pure function of the two streams.
// That is the substantive difference from codex, where the equivalent question
// ("is this text an answer?") can only be settled by reading the rollout, and
// where the tests consequently have to drive a real process.
// ---------------------------------------------------------------------------

/// The shape of a healthy run, trimmed to the fields we read.
const GROK_OK: &str = r#"{
  "text": "OK",
  "stopReason": "end_turn",
  "sessionId": "3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4",
  "usage": { "total_tokens": 22628 },
  "num_turns": 1
}"#;

#[test]
fn grok_end_turn_is_an_answer() {
    let super::GrokOutcome::Answered(answer) =
        super::interpret_grok_output(GROK_OK, "").expect("end_turn is an answer")
    else {
        panic!("end_turn must classify as an answer");
    };
    assert_eq!(answer.text, "OK");
    assert_eq!(
        answer.session_id.as_deref(),
        Some("3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4"),
        "the echoed session id is what we record, not the one we asked for"
    );
}

#[test]
fn grok_served_model_and_cost_are_read_from_model_usage() {
    // Trimmed from a real grok 1.0.40 run under the default `-m grok-4.7`: the
    // served name differs from anything `grok models` lists, which is why it is
    // recorded separately from `model` rather than replacing it.
    let out = r#"{
      "text": "ok",
      "stopReason": "end_turn",
      "sessionId": "3b8f1c22-9d4e-4a51-b7c6-2e0f9a8d7c11",
      "num_turns": 1,
      "total_cost_usd": 0.02397,
      "modelUsage": { "grok-4.7-build": { "modelCalls": 1, "costUSD": 0.02397 } }
    }"#;
    assert_eq!(
        super::result_served(out),
        super::Served {
            model: Some("grok-4.7-build".to_string()),
            cost_usd: Some(0.02397),
        }
    );
    // Absent fields and non-JSON record nothing rather than failing the run.
    assert_eq!(super::result_served(GROK_OK), super::Served::default());
    assert_eq!(super::result_served("not json"), super::Served::default());
}

/// claude's first `stream-json` event, trimmed.
const CLAUDE_INIT: &str = r#"{"type":"system","subtype":"init","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","model":"claude-opus-5-5"}"#;

/// A clean claude result event, trimmed from a real Claude Code 2.1.284 run.
const CLAUDE_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"LAUNCHED","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","num_turns":2,"terminal_reason":"completed","api_error_status":null,"total_cost_usd":0.21126,"usage":{"input_tokens":4,"cache_creation_input_tokens":25470,"cache_read_input_tokens":22520,"output_tokens":149,"output_tokens_details":{"thinking_tokens":0}},"modelUsage":{"claude-opus-5-5":{"costUSD":0.21126}}}"#;

/// `events` as claude's stream: one event per line.
fn claude_stream(events: &[&str]) -> String {
    events.join("\n") + "\n"
}

#[test]
fn claude_clean_result_is_an_answer() {
    let stdout = claude_stream(&[CLAUDE_INIT, CLAUDE_OK]);
    let outcome = super::interpret_claude_output(&stdout, "").expect("a result object");
    let super::ClaudeOutcome::Answered { text, session_id } = outcome else {
        panic!("is_error false is an answer");
    };
    assert_eq!(text, "LAUNCHED");
    assert_eq!(
        session_id.as_deref(),
        Some("2f8b2c2f-d939-4823-a615-7ebd6637f958")
    );
}

#[test]
fn claude_api_error_is_a_stated_failure_that_keeps_its_session() {
    // Real output for `--model no-such-model-xyz`: exit 1, and a result object
    // that still says `subtype: "success"` - only `is_error` gives it away. The
    // session exists on disk, so the id must survive; text mode discarded it.
    let unknown_model = claude_stream(&[
        CLAUDE_INIT,
        r#"{"type":"result","subtype":"success","is_error":true,"result":"There's an issue with the selected model (no-such-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model.","session_id":"ff2d6c14-1b45-481f-85a2-24e367d9e6ad","num_turns":1,"terminal_reason":"api_error","api_error_status":404,"total_cost_usd":0}"#,
    ]);
    let outcome = super::interpret_claude_output(
        &unknown_model,
        r#"[claude-code:unrecognized_model] {"model":"no-such-model-xyz"}"#,
    )
    .expect("an error result is still a result, not a launch failure");
    let super::ClaudeOutcome::NoAnswer {
        reason, session_id, ..
    } = outcome
    else {
        panic!("is_error true is not an answer, whatever the subtype says");
    };
    assert!(reason.contains("api_error"), "{reason}");
    assert!(reason.contains("404"), "{reason}");
    assert!(
        reason.contains("issue with the selected model"),
        "claude's own explanation is persisted: {reason}"
    );
    assert_eq!(
        session_id.as_deref(),
        Some("ff2d6c14-1b45-481f-85a2-24e367d9e6ad")
    );
}

#[test]
fn claude_result_without_is_error_fails_closed() {
    // A claude that stopped reporting the flag must not pass every run off as
    // answered.
    let unflagged = r#"{"type": "result", "result": "maybe", "session_id": "s"}"#;
    let outcome = super::interpret_claude_output(unflagged, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer { reason, .. } = outcome else {
        panic!("a result with no is_error is not trusted as an answer");
    };
    assert!(reason.contains("no is_error flag"), "{reason}");
}

#[test]
fn claude_output_without_a_result_object_is_a_launch_failure() {
    let err = super::interpret_claude_output("", "Error: not logged in")
        .expect_err("no result object: nothing ran");
    assert!(err.to_string().contains("not logged in"), "{err}");
    // A stream cut off after its first event is not a result either - the
    // shape a claude killed mid-turn leaves. (`run_claude` still keeps that
    // run's session: see `a_claude_that_dies_after_starting_keeps_its_session`.)
    let err = super::interpret_claude_output(&claude_stream(&[CLAUDE_INIT]), "")
        .expect_err("a system event is not a result");
    assert!(err.to_string().contains("no result object"), "{err}");
}

#[test]
fn claude_interrupted_turn_is_not_an_answer() {
    // Real shape of a SIGINT mid-command: exit 0, and a result that says
    // `error_during_execution` / `aborted_tools` with no `result` text at all.
    // Only `is_error` stops it reading as an empty answer.
    let stdout = claude_stream(&[
        CLAUDE_INIT,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"4397a6d5-4943-4b52-a0b6-d20c75078bd9","num_turns":3,"terminal_reason":"aborted_tools","stop_reason":"tool_use"}"#,
    ]);
    let outcome = super::interpret_claude_output(&stdout, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer {
        reason, session_id, ..
    } = outcome
    else {
        panic!("an aborted turn is not an answer");
    };
    assert!(reason.contains("aborted_tools"), "{reason}");
    assert_eq!(
        session_id.as_deref(),
        Some("4397a6d5-4943-4b52-a0b6-d20c75078bd9")
    );
}

#[test]
fn claude_last_result_in_the_stream_is_the_answer() {
    // A background subagent's result is handed back in a second turn, and the
    // stream then carries one result per turn: the first is the model's
    // "launched" note, the last its answer.
    let first = CLAUDE_OK;
    let last = r#"{"type":"result","subtype":"success","is_error":false,"result":"PROBE-B-DONE","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","num_turns":1}"#;
    let stdout = claude_stream(&[CLAUDE_INIT, first, CLAUDE_INIT, last]);
    let outcome = super::interpret_claude_output(&stdout, "").expect("a result object");
    let super::ClaudeOutcome::Answered { text, .. } = outcome else {
        panic!("the last result is an answer");
    };
    assert_eq!(text, "PROBE-B-DONE");
}

#[test]
fn claude_usage_and_served_are_read_from_the_result() {
    let (turns, usage) = super::claude_usage(CLAUDE_OK);
    assert_eq!(turns, 2);
    // Uncached input, cache writes and cache reads together, as codex counts
    // input; the reads are the cached share.
    assert_eq!(usage.input_tokens, 4 + 25470 + 22520);
    assert_eq!(usage.cached_input_tokens, 22520);
    assert_eq!(usage.output_tokens, 149);
    assert_eq!(
        super::result_served(CLAUDE_OK),
        super::Served {
            model: Some("claude-opus-5-5".to_string()),
            cost_usd: Some(0.21126),
        }
    );
    assert_eq!(super::claude_usage("not json").0, 0);
}

#[test]
fn grok_cancelled_turn_is_not_an_answer() {
    // Observed by running a tool-using prompt under `--max-turns 1`: a full
    // result object, exit 1, and `text` holding the model's opening remark.
    // Persisting that as the response is the exact failure this gate prevents.
    let cut_off = r#"{
      "text": "I'll list everything in `src/` first, then read each file.",
      "stopReason": "cancelled",
      "sessionId": "019ffbc5-2b48-7611-a7f2-48db1fa2b0ed",
      "num_turns": 1
    }"#;
    let outcome = super::interpret_grok_output(cut_off, "Error: max turns reached")
        .expect("a cancelled turn still ran - it is not a launch failure");
    let super::GrokOutcome::NoAnswer {
        reason,
        text,
        session_id,
    } = outcome
    else {
        panic!("a cancelled turn is not an answer");
    };
    assert!(
        reason.contains("cancelled"),
        "names the stop reason: {reason}"
    );
    assert!(
        text.contains("list everything"),
        "the interim commentary is kept as a clue: {text}"
    );
    // The session id is the point of returning this rather than an error: the
    // turn that was cut off is the one most worth resuming, and `invoke` drops
    // the id on every error path.
    assert_eq!(
        session_id.as_deref(),
        Some("019ffbc5-2b48-7611-a7f2-48db1fa2b0ed"),
        "a cut-off turn keeps its session id"
    );

    // It must still read as a failure everywhere that matters: exit code,
    // greppable digest, and no auto-resume (a stated reason is not a wedge).
    let digest = super::no_answer_digest(reason, Some(&exited(1)));
    assert_eq!(
        digest.exit_code,
        Some(1),
        "the digest reports the status we observed"
    );
    assert!(!digest.captured, "nothing was captured");
    assert!(
        !digest.recovered_from_transcript,
        "grok has no rollout to recover from"
    );
    assert!(
        digest.summary().turn_error.is_some(),
        "the reason persists flat, so these stay greppable"
    );
}

#[test]
fn grok_write_launch_keeps_a_resumed_session_on_its_recorded_profile() {
    use super::GrokWrite::{BuiltinWorkspace, Fresh, Resume};
    let name = crate::grok_home::profile_name(&["/home/u/.brokkr".to_string()]);
    // A fresh write run takes a new profile named for its derived roots.
    assert_eq!(
        super::grok_write_launch(true, Some("workspace")),
        Some(Fresh)
    );
    // Grok refuses a resume whose --sandbox differs from the session's saved
    // profile, so a session created under the built-in `workspace` (before
    // profiles) must resume as `workspace` - trusted, no extra roots ...
    assert_eq!(
        super::grok_write_launch(false, Some("workspace")),
        Some(BuiltinWorkspace)
    );
    // ... and one created under a `review-ws-*` profile keeps that exact one.
    assert_eq!(
        super::grok_write_launch(false, Some(&name)),
        Some(Resume(name.clone()))
    );
    // Anything else does not write.
    assert_eq!(super::grok_write_launch(true, Some("read-only")), None);
    assert_eq!(super::grok_write_launch(true, None), None);
}

#[test]
fn grok_turns_and_usage_are_read_from_the_result() {
    // Trimmed from a real grok 1.0.40 run. Before this was read, every grok
    // digest said 0 turns and 0 tokens, which reads as "nothing ran" on a run
    // that spent real money.
    let out = r#"{
      "text": "",
      "stopReason": "cancelled",
      "sessionId": "01a0c86b-18a3-7c70-9851-66dd509787b0",
      "usage": {
        "input_tokens": 6716,
        "cache_read_input_tokens": 30592,
        "output_tokens": 154,
        "reasoning_tokens": 82
      },
      "num_turns": 2
    }"#;
    let (turns, usage) = super::grok_usage(out);
    assert_eq!(turns, 2);
    assert_eq!(
        usage.input_tokens,
        6716 + 30592,
        "codex-shaped: input includes cached"
    );
    assert_eq!(usage.cached_input_tokens, 30592);
    assert_eq!(usage.output_tokens, 154);
    assert_eq!(usage.reasoning_output_tokens, 82);
    let (turns, usage) = super::grok_usage("not json");
    assert_eq!((turns, usage.input_tokens), (0, 0));
}

#[test]
fn grok_no_answer_reason_carries_the_cause_and_the_trust_hint() {
    let base = "grok ended the turn without an answer (stopReason: cancelled)".to_string();
    assert_eq!(
        super::explain_grok_no_answer(base.clone(), None, None),
        base
    );
    let full = super::explain_grok_no_answer(
        base,
        Some("permission_cancelled: permission for `search_replace` was refused"),
        Some("/p is not in grok's trusted folders"),
    );
    assert!(full.contains("stopReason: cancelled"), "{full}");
    assert!(full.contains("search_replace"), "{full}");
    assert!(full.contains("trusted folders"), "{full}");
}

/// An exit status with the given code, for digests that must report what was
/// observed rather than what was assumed.
fn exited(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

#[test]
fn grok_missing_stop_reason_is_not_an_answer() {
    // A future grok that drops or renames the field must not be read as
    // success: the whole gate rests on that field being present and known.
    // This object still identifies itself as a result (it carries a session
    // id), so the turn demonstrably ran - it just did not say how it ended.
    let outcome = super::interpret_grok_output(
        r#"{"text": "half a thought", "sessionId": "019ffbc5-2b48-7611-a7f2-48db1fa2b0ed"}"#,
        "",
    )
    .expect("an unlabelled turn still ran");
    let super::GrokOutcome::NoAnswer { reason, .. } = outcome else {
        panic!("an unlabelled turn is not a proven answer");
    };
    assert!(
        reason.contains("missing"),
        "says the reason was absent: {reason}"
    );
}

#[test]
fn json_that_is_not_a_result_object_is_a_launch_failure() {
    // Every field of a result object is optional, so *any* JSON object
    // deserializes into an all-default `GrokResult`. Reading that as "a turn
    // ran but did not answer" would infer the fact from successful parsing
    // rather than from evidence - and `NoAnswer` returns `Ok`, which tells
    // `review resume` the prompt cache was warmed. An unrecognised pre-turn failure
    // would then mark a cold session warm and get the *next* resume refused.
    let err = super::interpret_grok_output(r#"{"unexpected": "shape"}"#, "Error: something else")
        .expect_err("nothing here says a turn ran");
    let msg = err.to_string();
    assert!(
        msg.contains("no recognisable result"),
        "distinct from a labelled non-answer: {msg}"
    );
    assert!(msg.contains("something else"), "quotes the reason: {msg}");
}

#[test]
fn grok_without_a_result_object_reports_the_failure() {
    // Unknown model / unknown session / auth failure: grok never reaches a
    // turn, so there is no object to parse and the message is all we have.
    let err = super::interpret_grok_output("", "Error: Couldn't set model 'nope-9000'")
        .expect_err("no result object is a failure");
    let msg = err.to_string();
    assert!(msg.contains("no result object"), "distinct class: {msg}");
    assert!(msg.contains("nope-9000"), "quotes grok's reason: {msg}");
}

#[test]
fn grok_typed_error_line_keeps_its_message() {
    // Every field of a result object is optional, so this line parses as a
    // perfectly valid empty result. Read as one it reports only "no stop
    // reason" and throws away the sentence that says what went wrong - which is
    // the whole content of a pre-turn failure.
    let err = super::interpret_grok_output(r#"{"type":"error","message":"boom"}"#, "")
        .expect_err("a typed error line is not an answer");
    assert!(
        err.to_string().contains("boom"),
        "an error printed only on stdout keeps its message: {err}"
    );
}

/// A profile `config` entry restating the writable-roots key wins over the
/// derived one, because codex takes the last `-c`. The sidecar has to follow
/// the run rather than the derivation, or it makes a confident false claim
/// about what a run could write.
#[test]
fn a_profile_config_override_of_writable_roots_is_what_gets_recorded() {
    let config = vec![r#"sandbox_workspace_write.writable_roots=["/secret"]"#.to_string()];
    assert_eq!(
        super::config_writable_roots_override(&config),
        Some(vec!["/secret".to_string()])
    );
}

/// An explicit empty override is a real state - the profile narrowed the run
/// back to codex's defaults - and must not read as "no override".
#[test]
fn an_empty_writable_roots_override_is_recorded_as_empty_not_absent() {
    let config = vec!["sandbox_workspace_write.writable_roots=[]".to_string()];
    assert_eq!(super::config_writable_roots_override(&config), Some(vec![]));
}

#[test]
fn unrelated_profile_config_entries_leave_the_derived_roots_recorded() {
    let config = vec![
        r#"model_provider="openai-http""#.to_string(),
        r#"sandbox_workspace_write.network_access=true"#.to_string(),
    ];
    assert_eq!(super::config_writable_roots_override(&config), None);
}

/// Codex takes the last `-c` for a repeated key, so the record must too.
#[test]
fn the_last_writable_roots_override_wins() {
    let config = vec![
        r#"sandbox_workspace_write.writable_roots=["/first"]"#.to_string(),
        r#"sandbox_workspace_write.writable_roots=["/second"]"#.to_string(),
    ];
    assert_eq!(
        super::config_writable_roots_override(&config),
        Some(vec!["/second".to_string()])
    );
}

/// Every level but `read-only` selects the sandbox the plain way.
#[test]
fn a_writable_level_is_passed_as_the_sandbox_flag() {
    assert_eq!(
        super::codex_sandbox_args("workspace-write"),
        vec!["--sandbox".to_string(), "workspace-write".to_string()]
    );
    assert_eq!(
        super::codex_sandbox_args("danger-full-access"),
        vec!["--sandbox".to_string(), "danger-full-access".to_string()]
    );
}

/// An unrecognised level is passed through verbatim rather than rejected -
/// grok resolves custom profile names, and the provider validates its own
/// vocabulary with a better error than we could write.
#[test]
fn an_unknown_level_is_passed_through_as_a_sandbox_flag() {
    assert_eq!(
        super::codex_sandbox_args("custom-profile"),
        vec!["--sandbox".to_string(), "custom-profile".to_string()]
    );
}

/// `read-only` must NOT pass `--sandbox`: a `sandbox_mode` override makes
/// `resolve_permission_config_syntax` return `Legacy`, which renders the
/// `[permissions]` profile inert and silently drops the network grant.
#[test]
fn read_only_selects_a_permissions_profile_instead_of_the_sandbox_flag() {
    let args = super::codex_sandbox_args("read-only");
    assert!(
        !args.iter().any(|a| a == "--sandbox"),
        "--sandbox would force the legacy pipeline and void the profile: {args:?}"
    );
    assert!(
        args.iter()
            .any(|a| a == r#"permissions.review-read-only.extends=":read-only""#)
    );
    assert!(
        args.iter()
            .any(|a| a == "permissions.review-read-only.network.enabled=true")
    );
    assert!(
        args.iter()
            .any(|a| a == r#"default_permissions="review-read-only""#)
    );
}

/// The profile must extend the built-in `:read-only`, since that is what
/// carries the filesystem restriction the level advertises. Defining the
/// profile without a parent would grant network over an unrestricted
/// filesystem.
#[test]
fn the_read_only_profile_extends_the_builtin_read_only_filesystem() {
    let args = super::codex_sandbox_args("read-only");
    let extends = args
        .iter()
        .find(|a| a.contains(".extends="))
        .expect("profile must declare a parent");
    assert!(
        extends.ends_with(r#"=":read-only""#),
        "expected the built-in read-only parent, got {extends}"
    );
}

fn grant(path: &str, why: &'static str) -> crate::writable_roots::GrantedRoot {
    crate::writable_roots::GrantedRoot {
        path: path.to_string(),
        why,
    }
}

/// The ordinary case: no override, so every effective root is a derived one and
/// keeps the reason a build needs it.
#[test]
fn announcement_reports_derived_roots_with_their_reason() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    let lines = super::announcement_lines(
        "codex",
        "profile config override",
        &granted,
        &["/run/user/1000".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /run/user/1000 (build lock ($XDG_RUNTIME_DIR))"]
    );
}

/// The defect this function exists to prevent. A profile `config` override
/// replaces the roots the run launches with, and codex takes the last `-c`, so
/// announcing the derived set would print paths the run did not receive while
/// withholding the one it did.
#[test]
fn announcement_follows_a_config_override_rather_than_the_derivation() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    let lines = super::announcement_lines(
        "codex",
        "profile config override",
        &granted,
        &["/secret".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /secret (profile config override)"]
    );
    assert!(
        !lines.iter().any(|l| l.contains("/run/user/1000")),
        "a replaced derived root must not be announced as granted: {lines:?}"
    );
}

/// An override that *adds* to an empty derivation still has to be announced -
/// the old guard keyed on the derived set being non-empty and printed nothing
/// here, which is the silent-widening case the fence exists to catch.
#[test]
fn announcement_covers_roots_added_to_an_empty_derivation() {
    let lines = super::announcement_lines(
        "codex",
        "profile config override",
        &[],
        &["/srv/cache".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /srv/cache (profile config override)"]
    );
}

/// A reason belongs to a path, not to a position: an override that keeps one
/// derived root and adds another must label each correctly rather than pairing
/// them off in order.
#[test]
fn announcement_matches_reasons_by_path_not_by_position() {
    let granted = vec![
        grant("/media/disk/cargo", "cargo target ($CARGO_TARGET_DIR)"),
        grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)"),
    ];
    let effective = vec!["/run/user/1000".to_string(), "/srv/extra".to_string()];
    let lines = super::announcement_lines("codex", "profile config override", &granted, &effective);
    assert_eq!(
        lines,
        vec![
            "codex: writable root /run/user/1000 (build lock ($XDG_RUNTIME_DIR))",
            "codex: writable root /srv/extra (profile config override)",
        ]
    );
}

/// An explicit `[]` override widens nothing, so there is nothing to announce -
/// distinct from having derived nothing, which the separate zero-root warning
/// covers.
#[test]
fn announcement_is_empty_when_an_override_grants_nothing() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    assert!(
        super::announcement_lines("codex", "profile config override", &granted, &[]).is_empty()
    );
}
