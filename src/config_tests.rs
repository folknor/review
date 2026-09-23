//! Resolution tests across the local and global config layers.
//!
//! `config.rs`'s own `mod tests` covers parsing a single file; everything here
//! is about how two files combine: which one wins, whether a winner replaces or
//! merges, and whether the provenance (`layer`, `table`) that `review config`
//! reports is the provenance of the value actually used.

#[allow(clippy::unwrap_used)]
mod layering {
    use crate::config::*;

    fn local(raw: &str) -> ConfigFile {
        parse_file(raw, "local.toml", Layer::Local).unwrap()
    }

    fn global(raw: &str) -> ConfigFile {
        parse_file(raw, "global.toml", Layer::Global).unwrap()
    }

    fn layered(local_raw: &str, global_raw: &str, host: &str) -> Result<ReviewConfig> {
        resolve(local(local_raw), Some(global(global_raw)), host)
    }

    fn entry<'a>(cfg: &'a ReviewConfig, provider: &str, profile: &str) -> &'a ProfileEntry {
        &cfg.profiles[provider][profile]
    }

    // ---- profiles -------------------------------------------------------

    #[test]
    fn a_global_profile_applies_when_the_project_defines_none() {
        // The whole point of the global layer: which model serves a tier is
        // stated once per operator, not restated in every project.
        let cfg = layered("", "[codex.deep]\nmodel = \"g\"\n", "h").unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("g"));
        assert_eq!(e.effective.layer, Layer::Global);
        assert_eq!(e.effective.table, "[codex.deep]");
        assert_eq!(e.effective.host, None);
        assert!(e.shadowed.is_empty());
    }

    #[test]
    fn a_local_profile_replaces_the_global_one_whole() {
        // No field-level merge: the global profile opts up to workspace-write,
        // the local one only names a model. If fields merged, the project's
        // profile would silently inherit write access it never asked for.
        let cfg = layered(
            "[codex.deep]\nmodel = \"local-model\"\n",
            "[codex.deep]\nmodel = \"global-model\"\neffort = \"high\"\n\
             sandbox = \"workspace-write\"\nwritable_roots = [\"/srv\"]\n\
             config = ['a=\"b\"']\nenv = { K = \"v\" }\n",
            "h",
        )
        .unwrap();
        let p = cfg.resolve_profile("codex", "deep").unwrap();
        assert_eq!(p.model.as_deref(), Some("local-model"));
        assert_eq!(p.effort, None);
        assert_eq!(p.sandbox, None);
        assert!(p.writable_roots.is_empty());
        assert!(p.config.is_empty());
        assert!(p.env.is_none());
    }

    #[test]
    fn the_shadowed_global_profile_is_retained_for_display() {
        // `review config` shows what a stale local override is hiding; that
        // only works if the loser survives resolution intact.
        let cfg = layered(
            "[codex.deep]\nmodel = \"l\"\n",
            "[codex.deep]\nmodel = \"g\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.layer, Layer::Local);
        assert_eq!(e.shadowed.len(), 1);
        assert_eq!(e.shadowed[0].layer, Layer::Global);
        assert_eq!(e.shadowed[0].table, "[codex.deep]");
        assert_eq!(e.shadowed[0].profile.model.as_deref(), Some("g"));
    }

    #[test]
    fn profile_precedence_is_local_host_local_hostless_global_host_global_hostless() {
        let local_raw = "\
[codex.deep]
model = \"local-hostless\"

[h.codex.deep]
model = \"local-host\"
";
        let global_raw = "\
[codex.deep]
model = \"global-hostless\"

[h.codex.deep]
model = \"global-host\"
";
        let cfg = layered(local_raw, global_raw, "h").unwrap();
        let e = entry(&cfg, "codex", "deep");

        let order: Vec<(&str, Layer, &str, Option<&str>)> = std::iter::once(&e.effective)
            .chain(&e.shadowed)
            .map(|d| {
                (
                    d.profile.model.as_deref().unwrap(),
                    d.layer,
                    d.table.as_str(),
                    d.host.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("local-host", Layer::Local, "[h.codex.deep]", Some("h")),
                ("local-hostless", Layer::Local, "[codex.deep]", None),
                ("global-host", Layer::Global, "[h.codex.deep]", Some("h")),
                ("global-hostless", Layer::Global, "[codex.deep]", None),
            ]
        );
    }

    #[test]
    fn a_global_host_table_does_not_beat_a_local_hostless_one() {
        // Host-specificity only ranks definitions *within* a file. Across
        // files the layer decides, or a project could never override a host
        // table the operator wrote globally.
        let cfg = layered(
            "[codex.deep]\nmodel = \"local\"\n",
            "[h.codex.deep]\nmodel = \"global-host\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("local"));
        assert_eq!(e.shadowed[0].table, "[h.codex.deep]");
        assert_eq!(e.shadowed[0].layer, Layer::Global);
    }

    #[test]
    fn host_tables_for_other_hosts_are_invisible_in_both_layers() {
        // Neither effective nor shadowed: a profile written for another
        // machine is not a candidate here at all.
        let cfg = layered(
            "[other.codex.deep]\nmodel = \"l-other\"\n",
            "[other.codex.deep]\nmodel = \"g-other\"\n[codex.deep]\nmodel = \"g\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("g"));
        assert!(e.shadowed.is_empty());

        let cfg = layered(
            "[other.codex.deep]\nmodel = \"l-other\"\n",
            "[other.codex.deep]\nmodel = \"g-other\"\n",
            "h",
        )
        .unwrap();
        assert!(cfg.resolve_profile("codex", "deep").is_none());
    }

    #[test]
    fn profiles_resolve_independently_per_provider_and_name() {
        // A local override of one profile must not hide the global's other
        // profiles, nor the same name under a different provider.
        let cfg = layered(
            "[codex.deep]\nmodel = \"l\"\n",
            "[codex.deep]\nmodel = \"g\"\n[codex.fast]\nmodel = \"gf\"\n\
             [claude.deep]\nmodel = \"gc\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_profile("codex", "deep")
                .unwrap()
                .model
                .as_deref(),
            Some("l")
        );
        assert_eq!(
            cfg.resolve_profile("codex", "fast")
                .unwrap()
                .model
                .as_deref(),
            Some("gf")
        );
        let claude = entry(&cfg, "claude", "deep");
        assert_eq!(claude.effective.profile.model.as_deref(), Some("gc"));
        assert!(claude.shadowed.is_empty());
    }

    #[test]
    fn a_dotted_hostname_is_quoted_in_the_recorded_table() {
        // The table string is shown to the operator as where to go and edit;
        // unquoted, `[a.b.codex.deep]` names a different (nonexistent) table.
        let raw = "[\"box.lan\".codex.deep]\nmodel = \"m\"\n";
        let cfg = layered(raw, raw, "box.lan").unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.table, "[\"box.lan\".codex.deep]");
        assert_eq!(e.effective.host.as_deref(), Some("box.lan"));
        assert_eq!(e.effective.layer, Layer::Local);
        assert_eq!(e.shadowed[0].table, "[\"box.lan\".codex.deep]");
        assert_eq!(e.shadowed[0].layer, Layer::Global);
    }

    #[test]
    fn a_hostless_profile_resolves_the_same_on_every_host() {
        let g = "[grok.deep]\nmodel = \"m\"\n";
        for host in ["a", "b.c", "plantasjen"] {
            let cfg = layered("", g, host).unwrap();
            assert_eq!(
                cfg.resolve_profile("grok", "deep")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("m"),
                "{host}"
            );
        }
    }

    // ---- archetypes and groups ------------------------------------------

    #[test]
    fn archetypes_union_across_layers_and_local_wins_per_name() {
        let cfg = layered(
            "[archetypes]\nbugs = \"local bugs\"\nperf = \"local perf\"\n",
            "[archetypes]\nbugs = \"global bugs\"\nsecurity = \"global security\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.archetypes.len(), 3);
        assert_eq!(cfg.archetype("bugs"), Some("local bugs"));
        assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
        assert_eq!(cfg.archetypes["perf"].layer, Layer::Local);
        assert_eq!(cfg.archetype("security"), Some("global security"));
        assert_eq!(cfg.archetypes["security"].layer, Layer::Global);
        assert_eq!(cfg.archetypes["security"].table, "[archetypes]");
    }

    #[test]
    fn a_local_group_may_reference_a_global_archetype() {
        // Validation has to happen after merging: a project composing its own
        // sweep out of the operator's generic archetypes is the normal case.
        let cfg = layered(
            "[archetypes]\nperf = \"p\"\n[_groups]\nsweep = [\"perf\", \"security\"]\n",
            "[archetypes]\nsecurity = \"s\"\n",
            "h",
        )
        .unwrap();
        let sweep = &cfg.groups["sweep"];
        assert_eq!(sweep.value, vec!["perf", "security"]);
        assert_eq!(sweep.layer, Layer::Local);
        assert_eq!(sweep.table, "[_groups]");
    }

    #[test]
    fn groups_union_across_layers_and_a_local_group_replaces_the_global_list() {
        // Lists replace, never merge: the local sweep is exactly what the
        // project wrote, not the global members plus its own.
        let cfg = layered(
            "[_groups]\nsweep = [\"bugs\"]\n",
            "[archetypes]\nbugs = \"b\"\nsecurity = \"s\"\n\
             [_groups]\nsweep = [\"security\", \"bugs\"]\nall-sec = [\"security\"]\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.groups["sweep"].value, vec!["bugs"]);
        assert_eq!(cfg.groups["sweep"].layer, Layer::Local);
        assert_eq!(cfg.groups["all-sec"].value, vec!["security"]);
        assert_eq!(cfg.groups["all-sec"].layer, Layer::Global);
    }

    #[test]
    fn a_group_member_no_layer_defines_errors_and_names_the_groups_layer() {
        let err = layered(
            "[_groups]\nsweep = [\"bugs\", \"ghost\"]\n",
            "[archetypes]\nbugs = \"b\"\n",
            "h",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("'ghost'"), "{err}");
        assert!(err.contains("local config"), "{err}");
    }

    #[test]
    fn a_global_group_may_not_lean_on_a_project_archetype() {
        // The global file is read in every project. A global group naming an
        // archetype only this project defines would resolve here and be an
        // error in every other project, so it is refused at parse time.
        let err = parse_file(
            "[_groups]\nsweep = [\"bugs\"]\n",
            "global.toml",
            Layer::Global,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("'bugs'"), "{err}");
        assert!(err.contains("global.toml"), "{err}");
    }

    #[test]
    fn a_shadowed_global_group_is_still_validated() {
        // Resolution only checks the winning group. The global file must be
        // valid on its own regardless, or it breaks the first project that
        // does not happen to shadow the broken group.
        let result = parse_file(
            "[archetypes]\nbugs = \"b\"\n[_groups]\nsweep = [\"ghost\"]\n",
            "global.toml",
            Layer::Global,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_misspelled_profile_key_is_an_error() {
        // A profile wins whole, so a typo that parsed as an empty profile would
        // silently discard the global definition it shadows.
        let err = parse_file("[codex.deep]\nmodle = \"x\"\n", "local.toml", Layer::Local)
            .unwrap_err()
            .to_string();
        assert!(err.contains("modle"), "{err}");
    }

    #[test]
    fn a_local_archetype_beats_a_global_group_of_the_same_name() {
        // The project wins across files for every name. Adding a group to the
        // global file must not break a project that already has an archetype
        // of that name.
        let cfg = layered(
            "[archetypes]\nsweep = \"a\"\n",
            "[archetypes]\nbugs = \"b\"\n[_groups]\nsweep = [\"bugs\"]\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.archetype("sweep"), Some("a"));
        assert!(!cfg.groups.contains_key("sweep"));
    }

    #[test]
    fn a_local_group_beats_a_global_archetype_of_the_same_name() {
        let cfg = layered(
            "[archetypes]\nbugs = \"b\"\n[_groups]\nsweep = [\"bugs\"]\n",
            "[archetypes]\nsweep = \"a\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.groups["sweep"].layer, Layer::Local);
        assert!(cfg.archetype("sweep").is_none());
    }

    #[test]
    fn a_group_and_archetype_clash_within_one_file_errors() {
        // Within one file there is no layer to pick a winner by.
        let err = layered(
            "",
            "[archetypes]\nsweep = \"a\"\nbugs = \"b\"\n[_groups]\nsweep = [\"bugs\"]\n",
            "h",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("conflicts"), "{err}");
        assert!(err.contains("(global config)"), "{err}");
    }

    #[test]
    fn a_global_group_naming_an_archetype_a_project_group_hides_explains_why() {
        // The project group `bugs` hides the global archetype `bugs`, which the
        // global group `sweep` still names. The generic "unknown archetype"
        // message would point at a name that is plainly in the global file.
        let err = layered(
            "[archetypes]\nx = \"x\"\n[_groups]\nbugs = [\"x\"]\n",
            "[archetypes]\nbugs = \"b\"\n[_groups]\nsweep = [\"bugs\"]\n",
            "h",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hides the global archetype"), "{err}");
    }

    #[test]
    fn reserved_names_error_as_archetype_or_group_in_the_global_layer_too() {
        for name in RESERVED_NAMES {
            let raw = format!("[archetypes]\n{name} = \"x\"\n");
            let err = parse_file(&raw, "global.toml", Layer::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("reserved"), "{name}: {err}");
            assert!(err.contains("global.toml"), "{name}: {err}");

            let raw = format!("[archetypes]\nbugs = \"x\"\n[_groups]\n{name} = [\"bugs\"]\n");
            let err = parse_file(&raw, "global.toml", Layer::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
    }

    #[test]
    fn every_reserved_name_errors_as_a_local_group() {
        for name in RESERVED_NAMES {
            let raw = format!("[archetypes]\nbugs = \"x\"\n[_groups]\n{name} = [\"bugs\"]\n");
            let err = parse_file(&raw, "local.toml", Layer::Local)
                .unwrap_err()
                .to_string();
            assert!(err.contains("reserved"), "{name}: {err}");
        }
    }

    // ---- defaults -------------------------------------------------------

    #[test]
    fn a_local_provider_list_replaces_the_global_one() {
        let cfg = layered(
            "[_defaults]\nproviders = [\"grok\"]\n",
            "[_defaults]\nproviders = [\"claude\", \"codex\"]\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_providers().unwrap(), &["grok".to_string()]);
        let p = cfg.providers.as_ref().unwrap();
        assert_eq!(p.layer, Layer::Local);
        assert_eq!(p.table, "[_defaults]");
    }

    #[test]
    fn an_explicitly_empty_local_provider_list_still_wins() {
        // `providers = []` is the project saying "always pass --provider",
        // which must not be read as "unset" and filled from the global list.
        let cfg = layered(
            "[_defaults]\nproviders = []\n",
            "[_defaults]\nproviders = [\"codex\"]\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_providers().unwrap(), &[] as &[String]);
        assert_eq!(cfg.providers.as_ref().unwrap().layer, Layer::Local);
    }

    #[test]
    fn defaults_fall_through_key_by_key_not_table_by_table() {
        // A local `[_defaults]` that sets only stall_timeout_secs must not
        // blank the global provider list, and vice versa.
        let cfg = layered(
            "[_defaults]\nstall_timeout_secs = 0\n",
            "[_defaults]\nproviders = [\"codex\"]\nstall_timeout_secs = 900\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_providers().unwrap(), &["codex".to_string()]);
        assert_eq!(cfg.providers.as_ref().unwrap().layer, Layer::Global);
        // 0 means "disabled", which is a setting, not an absence.
        assert_eq!(cfg.stall_timeout_secs(), Some(0));
        assert_eq!(cfg.stall_timeout_secs.as_ref().unwrap().layer, Layer::Local);

        let cfg = layered(
            "[_defaults]\nproviders = [\"claude\"]\n",
            "[_defaults]\nstall_timeout_secs = 600\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_providers().unwrap(), &["claude".to_string()]);
        assert_eq!(cfg.stall_timeout_secs(), Some(600));
        let s = cfg.stall_timeout_secs.as_ref().unwrap();
        assert_eq!(s.layer, Layer::Global);
        assert_eq!(s.table, "[_defaults]");
    }

    #[test]
    fn defaults_are_none_when_no_layer_sets_them() {
        // Nothing is built in: an unset key stays unset rather than acquiring
        // a value `review config` would have to attribute to no file.
        let cfg = layered("[_defaults]\n", "[_defaults]\n", "h").unwrap();
        assert!(cfg.providers.is_none());
        assert!(cfg.stall_timeout_secs.is_none());
    }

    #[test]
    fn an_unknown_provider_in_global_defaults_errors() {
        let err = parse_file(
            "[_defaults]\nproviders = [\"codex\", \"kilo\"]\n",
            "global.toml",
            Layer::Global,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown provider 'kilo'"), "{err}");
        assert!(err.contains("global.toml"), "{err}");
    }

    // ---- audit ----------------------------------------------------------

    #[test]
    fn an_audit_block_in_the_global_config_errors() {
        // An audit id identifies one project; a global one would stamp every
        // project with the same id.
        let err = parse_file("[_audit]\nid = \"abcd\"\n", "global.toml", Layer::Global)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[_audit]"), "{err}");
        assert!(err.contains("global.toml"), "{err}");
    }

    #[test]
    fn the_resolved_audit_is_the_projects() {
        let cfg = layered(
            "[_audit]\nid = \"a1b2\"\nprivate = true\n",
            "[archetypes]\nbugs = \"b\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some("a1b2"));
        assert!(cfg.audit.private);
    }

    // ---- structural errors ----------------------------------------------

    #[test]
    fn an_unknown_provider_under_a_host_table_errors_in_either_layer() {
        for layer in [Layer::Local, Layer::Global] {
            let err = parse_file("[h.gpt.fast]\nmodel = \"m\"\n", "f.toml", layer)
                .unwrap_err()
                .to_string();
            assert!(err.contains("unknown provider 'gpt'"), "{err}");
        }
    }

    #[test]
    fn a_hostless_table_under_an_unknown_provider_errors() {
        // `[gpt.fast]` reads as host `gpt`, provider `fast`; it must still be
        // refused rather than silently ignored as another machine's profile.
        let err = parse_file("[gpt.fast]\nmodel = \"m\"\n", "f.toml", Layer::Global)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn a_non_table_top_level_value_errors() {
        for raw in ["stray = 1\n", "codex = \"deep\"\n", "h = [1]\n"] {
            let err = parse_file(raw, "f.toml", Layer::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("expected a table"), "{raw}: {err}");
        }
    }

    // ---- layer plumbing -------------------------------------------------

    #[test]
    fn an_absent_global_resolves_like_an_empty_one() {
        let raw = "\
[archetypes]
bugs = \"b\"
[_defaults]
providers = [\"codex\"]
[codex.deep]
model = \"m\"
";
        let without = resolve(local(raw), None, "h").unwrap();
        let with_empty = resolve(local(raw), Some(global("")), "h").unwrap();
        for cfg in [&without, &with_empty] {
            assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
            assert_eq!(cfg.providers.as_ref().unwrap().layer, Layer::Local);
            let e = entry(cfg, "codex", "deep");
            assert_eq!(e.effective.layer, Layer::Local);
            assert!(e.shadowed.is_empty());
        }
    }

    #[test]
    fn searched_lists_the_global_path_and_marks_it_when_absent() {
        // Error messages about an undefined archetype/profile quote this, so
        // the operator can see the global file was looked for but not found.
        let mut cfg = resolve(local(""), None, "h").unwrap();
        cfg.files = ConfigFiles {
            local: Some("/p/.review.toml".into()),
            global: Some("/home/u/.config/review/config.toml".into()),
            global_loaded: false,
        };
        assert_eq!(
            cfg.searched(),
            "/p/.review.toml, /home/u/.config/review/config.toml (absent)"
        );
        cfg.files.global_loaded = true;
        assert_eq!(
            cfg.searched(),
            "/p/.review.toml, /home/u/.config/review/config.toml"
        );
        cfg.files.global = None;
        assert_eq!(cfg.searched(), "/p/.review.toml");
        cfg.files.local = None;
        assert_eq!(cfg.searched(), "(no .review.toml)");
    }

    #[test]
    fn the_resolved_config_records_the_hostname_it_was_resolved_for() {
        let cfg = layered("", "", "box.lan").unwrap();
        assert_eq!(cfg.hostname, "box.lan");
    }
}
