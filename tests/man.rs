//! `review man` must work when nothing else does: it is where an agent goes to
//! learn how to repair a broken config, so it is dispatched before the project,
//! the config, stdin or any provider is looked at. Run as a child process
//! because the property is about the real dispatch order in `main`.

use std::process::Command;

/// Run from `/`, where no `.review.toml` is found walking up, so the no-project
/// condition holds wherever the build directory lives.
fn review(args: &[&str], xdg_config: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_review"))
        .args(args)
        .env("XDG_CONFIG_HOME", xdg_config)
        .current_dir("/")
        .output()
        .expect("review runs")
}

fn broken_global_config() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("man-broken-config");
    std::fs::create_dir_all(dir.join("review")).expect("scratch config dir");
    std::fs::write(dir.join("review/config.toml"), "this is = = not toml [")
        .expect("broken config written");
    dir
}

#[test]
fn man_works_with_a_broken_global_config_and_no_project() {
    let dir = broken_global_config();
    // The broken config is real: a command that reads it fails.
    assert!(!review(&["config"], &dir).status.success());

    let index = review(&["man"], &dir);
    assert!(index.status.success(), "{index:?}");
    assert!(String::from_utf8_lossy(&index.stdout).contains("sandbox"));

    // Whole and raw: exactly the file, nothing added, nothing rendered.
    let topic = review(&["man", "config"], &dir);
    assert!(topic.status.success(), "{topic:?}");
    let source = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/config.md"))
        .expect("docs/config.md");
    assert_eq!(topic.stdout, source);
}

#[test]
fn an_unknown_topic_fails_with_the_index_on_stderr_only() {
    let dir = broken_global_config();
    let index = review(&["man"], &dir).stdout;
    assert!(!index.is_empty());
    let out = review(&["man", "nope"], &dir);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "nothing on stdout: {out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(String::from_utf8_lossy(&index).as_ref()),
        "the whole index is on stderr: {err}"
    );
}

#[test]
fn man_refuses_project_selection_on_either_side() {
    let dir = broken_global_config();
    for args in [&["-C", "bz", "man"][..], &["man", "-C", "bz"]] {
        let out = review(args, &dir);
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        // The refusal itself, not a registry or config failure that happens
        // to fail the same way.
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("does not apply to `review man`"),
            "{args:?}: {err}"
        );
    }
}
