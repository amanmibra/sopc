//! Tests of the format, rendering, plans and analysis, on tests/fixtures/restaurants.
//! Command-line tests are in tests/cli.rs.

use crate::analyze::{compare, compare_json, lint, overlap, unit_spans, units};
use crate::model::*;
use crate::plan::{affected, make_plan, read_snapshot, snapshot};
use crate::render::{render_workspace, write_build, Build};
use crate::text::pretty_json;
use crate::workspace::{load, validate, Issue, Issues, Workspace};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn fixture() -> PathBuf {
    repo_root().join("tests/fixtures/restaurants")
}
fn sops() -> PathBuf {
    fixture().join("sops")
}
fn example() -> PathBuf {
    repo_root().join("examples/livekit-restaurant/sops")
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let p = e.unwrap().path();
        let target = dst.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &target);
        } else {
            std::fs::copy(&p, &target).unwrap();
        }
    }
}

/// A writable copy of the fixture's sops/ folder.
struct Repo(tempfile::TempDir);

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&sops(), dir.path());
        Repo(dir)
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.path().join(rel)
    }
    fn root(&self) -> &Path {
        self.0.path()
    }
    fn edit(&self, rel: &str, old: &str, new: &str) {
        let text = read(&self.path(rel));
        assert!(text.contains(old), "{old:?} not in {rel}");
        std::fs::write(self.path(rel), text.replace(old, new)).unwrap();
    }
    fn append(&self, rel: &str, text: &str) {
        let old = read(&self.path(rel));
        std::fs::write(self.path(rel), format!("{}{text}", old.trim_end())).unwrap();
    }
}

fn ws(root: &Path) -> Workspace {
    load(root).unwrap()
}
fn build(root: &Path) -> Build {
    render_workspace(&ws(root)).unwrap()
}
fn load_issues(root: &Path) -> Vec<Issue> {
    load(root).unwrap_err().downcast::<Issues>().unwrap().0
}
fn codes(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|i| i.code).collect()
}
/// The issues that stop rendering.
fn render_codes(root: &Path) -> Vec<&'static str> {
    render_workspace(&ws(root)).err().unwrap().0.iter().map(|i| i.code).collect()
}

// --- validation ------------------------------------------------------------------------------------

#[test]
fn fixture_is_valid() {
    assert!(validate(&ws(&sops())).is_empty());
}

#[test]
fn locked_is_no_longer_a_field_and_says_to_migrate() {
    let r = Repo::new();
    std::fs::write(
        r.path("instructions/brand-voice.md"),
        format!("---\nlocked: true\n---\n{}", read(&sops().join("instructions/brand-voice.md"))),
    )
    .unwrap();
    r.edit("procedures/reservations.yaml", "name: Reservations\n", "name: Reservations\nlocked: true\n");
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["removed_field", "removed_field"]);
    assert_eq!(issues[0].path, "instructions/brand-voice.md");
    assert_eq!(
        issues[0].message,
        "`locked` is no longer part of the format; run `sopc migrate` to remove it from this folder (or delete the line)"
    );
    assert_eq!(issues[1].path, "procedures/reservations.yaml");
    // A Markdown SOP's front matter: see markdown_formatting_checks_report_code_and_line.
}

#[test]
fn a_block_can_be_left_out() {
    let r = Repo::new();
    r.edit("agents/sakura-sushi.yaml", "  - closing\n", "");
    assert!(!build(r.root()).agents["sakura-sushi"].blocks.iter().any(|b| b.id() == "closing"));
}

#[test]
fn unknown_block() {
    let r = Repo::new();
    r.edit("agents/tonys-pizza.yaml", "  - pizza-context\n", "  - pasta-context\n");
    let issues = render_workspace(&ws(r.root())).err().unwrap().0;
    assert_eq!(codes(&issues), ["unknown_block"]);
    assert_eq!(issues[0].message, "blocks lists 'pasta-context', which is not an instruction, SOP or group");
}

#[test]
fn groups_expand_in_place_and_nest() {
    let r = Repo::new();
    r.append(
        "sopc.yaml",
        "\ngroups:\n  voice: [brand-voice]\n  host:\n    - restaurant-host\n    - voice\n  ordering:\n    - allergen-check\n    - delivery-handling\n",
    );
    let before = build(r.root());
    r.edit(
        "agents/luigis-trattoria.yaml",
        "  - restaurant-host\n  - brand-voice\n  - allergen-check\n  - delivery-handling\n",
        "  - host\n  - ordering\n",
    );
    let after = build(r.root());
    assert_eq!(after.agents["luigis-trattoria"].prompt, before.agents["luigis-trattoria"].prompt);
    let ids: Vec<&str> = after.agents["luigis-trattoria"].blocks.iter().map(|b| b.id()).collect();
    assert_eq!(
        ids,
        [
            "restaurant-host",
            "brand-voice",
            "allergen-check",
            "delivery-handling",
            "large-orders",
            "reservations",
            "closing"
        ]
    );
}

#[test]
fn group_problems() {
    let group = |yaml: &str| {
        let r = Repo::new();
        r.append("sopc.yaml", &format!("\ngroups:\n{yaml}"));
        let issues = validate(&ws(r.root()));
        issues.into_iter().filter(|i| !i.warning).map(|i| (i.code, i.message)).collect::<Vec<_>>()
    };
    let cycle = group("  a: [b]\n  b: [c]\n  c: [a]\n");
    assert_eq!(cycle.len(), 1, "{cycle:?}");
    assert_eq!(cycle[0], ("group_cycle", "groups contain each other in a loop: a → b → c → a".to_string()));
    assert_eq!(group("  self: [self]\n")[0].0, "group_cycle");
    let unknown = group("  a: [brand-voice, nope]\n");
    assert_eq!(
        unknown,
        [("unknown_block", "group 'a' lists 'nope', which is not an instruction, SOP or group".to_string())]
    );
    assert_eq!(group("  closing: [brand-voice]\n")[0].0, "duplicate_id");
}

#[test]
fn a_block_twice_in_an_agent_is_an_error() {
    let r = Repo::new();
    r.append("sopc.yaml", "\ngroups:\n  core:\n    - brand-voice\n    - allergen-check\n");
    r.edit("agents/tonys-pizza.yaml", "  - delivery-handling\n", "  - core\n  - delivery-handling\n");
    let issues = render_workspace(&ws(r.root())).err().unwrap().0;
    assert_eq!(codes(&issues), ["duplicate_block", "duplicate_block"]);
    assert_eq!(
        issues[0].message,
        "'brand-voice' appears twice in blocks (directly and in group `core`); list each block once"
    );
    let r = Repo::new();
    r.edit("agents/sakura-sushi.yaml", "  - closing\n", "  - closing\n  - closing\n");
    assert_eq!(render_codes(r.root()), ["duplicate_block"]);
}

#[test]
fn blocks_can_be_an_inline_list_or_a_bullet_list() {
    let r = Repo::new();
    let before = build(r.root());
    r.edit(
        "agents/sakura-sushi.yaml",
        "blocks:\n  - restaurant-host\n  - brand-voice\n  - allergen-check\n  - reservations\n  - closing\n",
        "blocks: [restaurant-host, brand-voice, allergen-check, reservations, closing]\n",
    );
    r.append("sopc.yaml", "\ngroups:\n  a: [brand-voice, closing]\n  b:\n    - brand-voice\n    - closing\n");
    let after = build(r.root());
    assert_eq!(after.agents["sakura-sushi"].prompt, before.agents["sakura-sushi"].prompt);
    let w = ws(r.root());
    assert_eq!(w.config.group("a"), w.config.group("b"));
}

#[test]
fn unused_block_is_a_warning() {
    let r = Repo::new();
    r.edit("agents/sakura-sushi.yaml", "  - reservations\n", "");
    r.edit("agents/luigis-trattoria.yaml", "  - reservations\n", "");
    let b = build(r.root());
    assert_eq!(codes(&b.warnings), ["unused_block"]);
    assert_eq!(b.warnings[0].path, "procedures/reservations.yaml");
}

#[test]
fn old_format_fields_say_to_migrate() {
    let check = |rel: &str, old: &str, new: &str, field: &str| {
        let r = Repo::new();
        r.edit(rel, old, new);
        let issues = load_issues(r.root());
        assert_eq!(codes(&issues), ["old_format"], "{rel}");
        assert_eq!(issues[0].path, rel);
        assert_eq!(
            issues[0].message,
            format!("{field} is from the old format; run `sopc migrate` to convert this folder")
        );
    };
    check("agents/tonys-pizza.yaml", "context: |", "inherits: [pizza-context]\ncontext: |", "`inherits`");
    check("agents/tonys-pizza.yaml", "context: |", "exclude: [closing]\ncontext: |", "`exclude`");
    check("agents/tonys-pizza.yaml", "context: |", "instructions: |", "`instructions`");
    check("procedures/reservations.yaml", "name: Reservations", "name: Reservations\nagents: \"*\"", "`agents`");
    check("procedures/reservations.yaml", "name: Reservations", "name: Reservations\nexclude: [x]", "`exclude`");
    check("sopc.yaml", "version: 1", "version: 1\nsop_order: [allergen-check]", "`sop_order`");
    for field in ["agents: \"*\"", "exclude: [x]", "inherits: [x]", "position: bottom"] {
        let name = field.split(':').next().unwrap();
        check("instructions/closing.md", "Before", &format!("---\n{field}\n---\nBefore"), &format!("`{name}`"));
    }
    let r = Repo::new();
    std::fs::write(r.path("procedures/x.md"), format!("---\nagents: \"*\"\n---\n{MD_OK}")).unwrap();
    assert_eq!(codes(&load_issues(r.root())), ["old_format"]);
    let r = Repo::new();
    std::fs::create_dir(r.path("bases")).unwrap();
    std::fs::rename(r.path("instructions/closing.md"), r.path("bases/closing.md")).unwrap();
    let issues = load_issues(r.root());
    assert_eq!((issues[0].code, issues[0].path.as_str()), ("old_format", "bases"));
    assert!(issues[0].message.contains("run `sopc migrate`"));
}

#[test]
fn retell_agents() {
    let r = Repo::new();
    let before = build(r.root());
    r.edit("agents/sakura-sushi.yaml", "livekit: sakura-sushi", "retell: agent_7c1");
    let after = build(r.root());
    assert_eq!(after.lock()["agents"]["sakura-sushi"]["platform_ref"], "retell:agent_7c1");
    assert!(changed_agents(&before, &after).is_empty());
}

#[test]
fn unset_variable() {
    let r = Repo::new();
    r.edit("agents/luigis-trattoria.yaml", "  menu_allergen_link: luigis.com/menu#allergens\n", "");
    assert_eq!(render_codes(r.root()), ["unset_variable"]);
}

#[test]
fn duplicate_platform_ref() {
    let r = Repo::new();
    r.edit("agents/sakura-sushi.yaml", "livekit: sakura-sushi", "livekit: tonys-pizza");
    assert_eq!(render_codes(r.root()), ["duplicate_platform_ref"]);
}

#[test]
fn agent_needs_exactly_one_platform() {
    let r = Repo::new();
    r.edit("agents/sakura-sushi.yaml", "livekit: sakura-sushi", "livekit: sakura-sushi\nvapi: asst_123");
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["invalid_field"]);
    assert_eq!(issues[0].path, "agents/sakura-sushi.yaml");
}

#[test]
fn explicit_id_must_match_file_name() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "name: Reservations", "id: bookings\nname: Reservations");
    assert_eq!(codes(&load_issues(r.root())), ["id_mismatch"]);
}

#[test]
fn unknown_field_is_rejected() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "name: Reservations", "name: Reservations\nsteps: []");
    assert_eq!(codes(&load_issues(r.root())), ["invalid_field"]);
}

fn rule(text: &str) -> Step {
    Step { plain: true, text: text.into(), ..Step::default() }
}

fn sop_with(steps: &[&str], nevers: &[&str]) -> Sop {
    Sop {
        id: "returns".into(),
        procedure_steps: steps.iter().map(|t| rule(t)).collect(),
        forbidden_actions: nevers.iter().map(|t| rule(t)).collect(),
        ..Sop::default()
    }
}

#[test]
fn rule_ids_follow_the_spec() {
    // sha256("allergen-check\x1fstep\x1fAsk if anyone in the order has a food allergy")[..12]
    assert_eq!(rule_id("allergen-check", "step", "Ask if anyone in the order has a food allergy"), "5cc0634f287a");
    let rules = sop_with(&["Ask for the order", "Read it back"], &["Never guess"]).rules();
    let summary: Vec<(&str, usize, &str)> = rules.iter().map(|r| (r.kind, r.index, r.text.as_str())).collect();
    assert_eq!(summary, [("step", 1, "Ask for the order"), ("step", 2, "Read it back"), ("never", 1, "Never guess")]);
    assert!(rules.iter().all(|r| r.id.len() == 12 && r.id.chars().all(|c| c.is_ascii_hexdigit())));
}

#[test]
fn rule_ids_ignore_whitespace_but_not_text() {
    let id = rule_id("returns", "step", "Ask for the order number");
    assert_eq!(rule_id("returns", "step", "  Ask  for the\n  order\tnumber \n"), id);
    assert_ne!(rule_id("returns", "step", "Ask for the order number."), id);
    assert_ne!(rule_id("returns", "step", "ask for the order number"), id);
    assert_ne!(rule_id("returns", "never", "Ask for the order number"), id, "the kind is part of the id");
    assert_ne!(rule_id("refunds", "step", "Ask for the order number"), id, "the procedure is part of the id");
    // Placeholders are hashed as written, so every agent gets the same id.
    assert_ne!(rule_id("returns", "step", "Call {{store_phone}}"), rule_id("returns", "step", "Call 555-0100"));
}

#[test]
fn rule_ids_survive_reordering() {
    let ids = |sop: Sop| -> BTreeMap<String, String> { sop.rules().into_iter().map(|r| (r.text, r.id)).collect() };
    let before = ids(sop_with(&["A", "B", "C"], &["Never D"]));
    let after = ids(sop_with(&["C", "A", "B"], &["Never D"]));
    assert_eq!(before, after);
}

#[test]
fn a_rule_twice_in_a_procedure_is_an_error() {
    let r = Repo::new();
    r.edit(
        "procedures/allergen-check.yaml",
        "  - Name the specific allergen back to the customer\n",
        "  - Name the specific allergen back to the customer\n  - Ask if anyone  in the order has a food allergy\n",
    );
    let issues = validate(&ws(r.root()));
    assert_eq!(codes(&issues), ["duplicate_rule"]);
    assert_eq!(issues[0].message, "step 1 and step 3 have the same text; remove one");
    // The same text as a step and as a never is two rules.
    let r = Repo::new();
    r.edit(
        "procedures/allergen-check.yaml",
        "  - Never say an item",
        "  - Ask if anyone in the order has a food allergy\n  - Never say an item",
    );
    assert!(validate(&ws(r.root())).is_empty());
}

#[test]
fn lock_and_export_list_each_procedures_rules() {
    let lock = build(&sops()).lock();
    let blocks = lock["agents"]["sakura-sushi"]["blocks"].as_array().unwrap();
    let allergen = blocks.iter().find(|b| b["id"] == "allergen-check").unwrap();
    let rules = allergen["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 6);
    assert_eq!(
        rules[0],
        serde_json::json!({"id": "5cc0634f287a", "kind": "step", "index": 1,
            "text": "Ask if anyone in the order has a food allergy"})
    );
    assert_eq!((&rules[3]["kind"], &rules[3]["index"]), (&serde_json::json!("never"), &serde_json::json!(1)));
    assert!(blocks.iter().filter(|b| b["kind"] != "sop").all(|b| b.get("rules").is_none()));
    // The same procedure has the same rules in every agent, and in export.
    let tonys = lock["agents"]["tonys-pizza"]["blocks"].as_array().unwrap();
    assert_eq!(tonys.iter().find(|b| b["id"] == "allergen-check").unwrap()["rules"], allergen["rules"]);
    let export = crate::workspace::export(&ws(&sops()));
    let sop = export["sops"].as_array().unwrap().iter().find(|s| s["id"] == "allergen-check").unwrap();
    assert_eq!(sop["rules"], allergen["rules"]);
}

#[test]
fn missing_goal_is_a_warning() {
    let r = Repo::new();
    r.edit(
        "procedures/reservations.yaml",
        "description: The customer has a confirmed table, or knows exactly why one isn't available.\n",
        "",
    );
    let b = build(r.root());
    assert_eq!(b.warnings.len(), 1);
    assert_eq!((b.warnings[0].code, b.warnings[0].warning), ("missing_goal", true));
}

#[test]
fn colon_in_step_gets_a_clear_fix() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "  - Never double-book a table", "  - Never say: we're fully booked");
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["colon_in_step"]);
    assert_eq!(issues[0].path, "procedures/reservations.yaml");
    assert!(issues[0].message.contains("forbiddenActions[0]"));
    assert!(issues[0].message.contains(r#"- "Never say: we're fully booked""#));
}

#[test]
fn quoted_colon_step_is_fine() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "  - Never double-book a table", r#"  - "Never say: fully booked""#);
    assert!(build(r.root()).agents["sakura-sushi"].prompt.contains("- Never say: fully booked"));
}

#[test]
fn unquoted_number_or_boolean_and_empty_steps() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "  - Never double-book a table", "  - 10\n  - true\n  -");
    assert_eq!(codes(&load_issues(r.root())), ["unquoted_value", "unquoted_value", "empty_step"]);
}

#[test]
fn yaml_1_2_reads_yes_no_and_times_as_text() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "  - Never double-book a table", "  - no\n  - 1:30\n  - On");
    let prompt = build(r.root()).agents["sakura-sushi"].prompt.clone();
    assert!(prompt.contains("- no\n- 1:30\n- On\n"), "{prompt}");
}

#[test]
fn colon_in_unquoted_field_gets_a_hint() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "scope: The customer wants", "scope: Note: the customer wants");
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["invalid_yaml"]);
    assert!(issues[0].message.contains("put the text in quotes"), "{}", issues[0].message);
}

#[test]
fn missing_config() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(codes(&load_issues(dir.path())), ["missing_config"]);
}

#[test]
fn legacy_config_name_says_to_rename_it() {
    let r = Repo::new();
    std::fs::rename(r.root().join("sopc.yaml"), r.root().join("opensop.yaml")).unwrap();
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["missing_config"]);
    assert!(issues[0].message.contains("rename opensop.yaml to sopc.yaml"), "{}", issues[0].message);
}

// --- rendering -------------------------------------------------------------------------------------

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn build_matches_golden_output() {
    let out = tempfile::tempdir().unwrap();
    write_build(&build(&sops()), out.path(), false).unwrap();
    let expected = fixture().join("expected");
    assert_eq!(file_names(out.path()), file_names(&expected));
    for name in file_names(&expected) {
        assert_eq!(read(&out.path().join(&name)), read(&expected.join(&name)), "{name} differs from the golden output");
    }
}

#[test]
fn example_build_is_current() {
    let out = tempfile::tempdir().unwrap();
    write_build(&build(&example()), out.path(), false).unwrap();
    let committed = example().join("build");
    assert_eq!(file_names(out.path()), file_names(&committed));
    for name in file_names(&committed) {
        assert_eq!(
            read(&out.path().join(&name)),
            read(&committed.join(&name)),
            "{name} differs from the example build"
        );
    }
}

fn headings(prompt: &str) -> Vec<&str> {
    prompt.lines().filter_map(|l| l.strip_prefix("### ")).collect()
}

#[test]
fn context_first_then_blocks_in_list_order() {
    let b = build(&sops());
    let tonys = &b.agents["tonys-pizza"];
    assert_eq!(tonys.instruction_ids(), ["restaurant-host", "pizza-context", "brand-voice", "closing"]);
    let at = |s: &str| tonys.prompt.find(s).unwrap();
    assert!(tonys.prompt.starts_with("Tony's is a wood-fired pizza shop"));
    assert!(at("wood-fired") < at("phone host") && at("phone host") < at("12\" and 16\""));
    assert!(at("12\" and 16\"") < at("Speak warmly") && at("Speak warmly") < at("## Procedures"));
    assert!(tonys.prompt.trim_end().ends_with("pickup or delivery time."));
}

#[test]
fn sops_heading_goes_once_before_the_first_sop() {
    let r = Repo::new();
    // An instruction between SOPs stays where it's listed, and the heading isn't repeated.
    r.edit("agents/sakura-sushi.yaml", "  - reservations\n  - closing\n", "  - closing\n  - reservations\n");
    let prompt = build(r.root()).agents["sakura-sushi"].prompt.clone();
    assert_eq!(prompt.matches("## Procedures").count(), 1);
    let at = |s: &str| prompt.find(s).unwrap();
    assert!(at("## Procedures") < at("### Allergen check") && at("Before hanging up") < at("### Reservations"));
    // An SOP first: the heading comes right after the context.
    r.edit("agents/sakura-sushi.yaml", "  - restaurant-host\n", "");
    r.edit("agents/sakura-sushi.yaml", "  - closing\n", "  - restaurant-host\n  - closing\n");
    r.edit(
        "agents/sakura-sushi.yaml",
        "  - brand-voice\n  - allergen-check\n",
        "  - allergen-check\n  - brand-voice\n",
    );
    let prompt = build(r.root()).agents["sakura-sushi"].prompt.clone();
    assert!(prompt.starts_with("Sakura is an omakase and sushi counter in Manhattan. Reservations strongly recommended. No delivery.\n\n## Procedures\n\n### Allergen check\n"), "{prompt}");
    // An empty heading leaves it out.
    r.append("sopc.yaml", "\nsops_heading: \"\"\n");
    let prompt = build(r.root()).agents["sakura-sushi"].prompt.clone();
    assert!(!prompt.contains("## Procedures") && prompt.contains("No delivery.\n\n### Allergen check\n"), "{prompt}");
}

#[test]
fn sops_in_list_order() {
    let b = build(&sops());
    assert_eq!(headings(&b.agents["tonys-pizza"].prompt), ["Allergen check", "Delivery", "Large orders"]);
    assert_eq!(
        headings(&b.agents["luigis-trattoria"].prompt),
        ["Allergen check", "Delivery", "Large orders", "Reservations"]
    );
    assert_eq!(headings(&b.agents["sakura-sushi"].prompt), ["Allergen check", "Reservations"]);
}

#[test]
fn variables_use_agent_values_over_workspace_defaults() {
    let b = build(&sops());
    assert!(b.agents["tonys-pizza"].prompt.contains("tonys.com/allergens"));
    assert!(b.agents["sakura-sushi"].prompt.contains("transfer to the head chef"));
    assert!(b.agents["tonys-pizza"].prompt.contains("transfer to the manager on duty"));
    assert!(b.agents.values().all(|r| !r.prompt.contains("{{")));
}

#[test]
fn auto_delivery_keeps_guards_in_prompt_and_steps_in_tool() {
    let b = build(&sops());
    let luigis = &b.agents["luigis-trattoria"];
    assert!(luigis.prompt.contains("Never promise a pickup time"));
    assert!(!luigis.prompt.contains("Check kitchen capacity"));
    assert!(luigis.prompt.contains("id `large-orders`"));
    let payload = &luigis.tool_payload["large-orders"];
    assert_eq!(
        payload["procedureSteps"][1],
        serde_json::json!({"text": "Check kitchen capacity for the requested time", "tool": "check_capacity"})
    );
    assert!(payload["text"].as_str().unwrap().contains("transfer to the manager on duty"));
    assert!(b.agents["sakura-sushi"].tool_payload.is_empty());
}

#[test]
fn tool_steps_render_as_instructions_and_are_listed() {
    let b = build(&sops());
    let tonys = &b.agents["tonys-pizza"];
    assert!(tonys.prompt.contains("Use the `lookup_allergens` tool."));
    assert!(tonys.prompt.contains("This applies to the `place_order` tool."));
    assert_eq!(
        tonys.tools,
        ["check_capacity", "check_delivery_zone", "lookup_allergens", "place_order", "transfer_to_staff"]
    );
}

fn changed_agents(a: &Build, b: &Build) -> Vec<String> {
    a.agents.keys().filter(|id| a.agents[*id].hash() != b.agents[*id].hash()).cloned().collect()
}

#[test]
fn editing_a_shared_instruction_changes_every_agent_that_uses_it() {
    let r = Repo::new();
    let before = build(r.root());
    r.edit("instructions/pizza-context.md", "12\" and 16\"", "10\", 12\" and 16\"");
    let after = build(r.root());
    assert_eq!(changed_agents(&before, &after), ["tonys-pizza"]);
    r.edit("instructions/brand-voice.md", "briefly", "concisely");
    let last = build(r.root());
    assert_eq!(changed_agents(&after, &last), before.agents.keys().cloned().collect::<Vec<_>>());
}

#[test]
fn lock_lists_blocks_and_tools() {
    let lock = build(&sops()).lock();
    let sakura = &lock["agents"]["sakura-sushi"];
    assert_eq!(sakura["platform_ref"], "livekit:sakura-sushi");
    let blocks: Vec<String> = sakura["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| format!("{}:{}", b["kind"].as_str().unwrap(), b["id"].as_str().unwrap()))
        .collect();
    assert_eq!(
        blocks,
        [
            "agent:sakura-sushi",
            "instruction:restaurant-host",
            "instruction:brand-voice",
            "sop:allergen-check",
            "sop:reservations",
            "instruction:closing"
        ]
    );
    assert!(!sakura["tools"].as_array().unwrap().contains(&serde_json::json!("check_delivery_zone")));
}

#[test]
fn write_build_removes_only_what_the_last_build_made() {
    let out = tempfile::tempdir().unwrap();
    let path = |name: &str| out.path().join(name);
    std::fs::write(path("lock.json"), r#"{"version": 1, "agents": {"old-agent": {}}}"#).unwrap();
    for name in ["old-agent.prompt.md", "old-agent.tool.json", "notes.prompt.md"] {
        std::fs::write(path(name), "x").unwrap();
    }
    write_build(&build(&sops()), out.path(), false).unwrap();
    assert!(!path("old-agent.prompt.md").exists() && !path("old-agent.tool.json").exists());
    assert_eq!(read(&path("notes.prompt.md")), "x", "not in the old lock.json: kept");
    assert!(read(&path("lock.json")).contains("\"version\": 1"));
    let parent = out.path().parent().unwrap();
    let tmp = format!(".{}.tmp", out.path().file_name().unwrap().to_string_lossy());
    assert!(!parent.join(tmp).exists(), "the temp folder is removed");

    // A folder with files but no lock.json is refused, unless forced; its files are kept.
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("notes.md"), "mine").unwrap();
    let err = write_build(&build(&sops()), other.path(), false).unwrap_err().to_string();
    assert!(err.contains("has files but no lock.json"), "{err}");
    assert_eq!(file_names(other.path()), ["notes.md"]);
    write_build(&build(&sops()), other.path(), true).unwrap();
    assert!(other.path().join("lock.json").exists());
    assert_eq!(read(&other.path().join("notes.md")), "mine");
}

#[test]
fn canonical_json_follows_field_order_and_keeps_non_ascii() {
    let instruction = Instruction { id: "b".into(), text: "Olá \"x\"\n".into() };
    assert_eq!(instruction.canonical_json(), r#"{"id":"b","text":"Olá \"x\"\n"}"#);
    let keys = |json: String| -> Vec<String> {
        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).unwrap().keys().cloned().collect()
    };
    assert_eq!(keys(instruction.canonical_json()), INSTRUCTION_FIELDS);
    assert_eq!(keys(Sop::default().canonical_json()), SOP_FIELDS);
    assert_eq!(keys(Agent::default().canonical_json()), AGENT_FIELDS);
}

// --- plan and affected ------------------------------------------------------------------------------

fn groups(plan: &crate::plan::Plan) -> Vec<(String, &'static str, Vec<String>)> {
    plan.by_block().into_iter().map(|((b, c), a)| (b, c, a)).collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plan_attributes_changes_to_blocks() {
    let r = Repo::new();
    let before = snapshot(&build(r.root()));
    r.edit("instructions/brand-voice.md", "briefly", "concisely");
    r.edit("procedures/reservations.yaml", "Never double-book a table", "Never double-book or overbook a table");
    let plan = make_plan(&before, &snapshot(&build(r.root())));
    let ids: Vec<&str> = plan.changes.iter().map(|c| c.agent.as_str()).collect();
    assert_eq!(ids, ["luigis-trattoria", "sakura-sushi", "tonys-pizza"]);
    assert_eq!(
        groups(&plan),
        [
            ("instruction:brand-voice".into(), "edited", strings(&["luigis-trattoria", "sakura-sushi", "tonys-pizza"])),
            ("sop:reservations".into(), "edited", strings(&["luigis-trattoria", "sakura-sushi"])),
        ]
    );
    assert!(plan.text(true).contains("instruction `brand-voice` edited → 3 agents"));
    assert!(plan.changes[0].diff.contains("-Speak warmly and briefly."));
    assert!(plan.changes[0].diff.contains("+Speak warmly and concisely."));
}

#[test]
fn plan_reports_blocks_an_agent_adds_or_removes() {
    let r = Repo::new();
    let before = snapshot(&build(r.root()));
    r.edit("agents/luigis-trattoria.yaml", "  - reservations\n", "");
    r.edit("agents/sakura-sushi.yaml", "  - closing\n", "  - closing\n  - pizza-context\n");
    let plan = make_plan(&before, &snapshot(&build(r.root())));
    assert_eq!(
        groups(&plan),
        [
            ("agent:luigis-trattoria".into(), "edited", strings(&["luigis-trattoria"])),
            ("agent:sakura-sushi".into(), "edited", strings(&["sakura-sushi"])),
            ("instruction:pizza-context".into(), "added", strings(&["sakura-sushi"])),
            ("sop:reservations".into(), "removed", strings(&["luigis-trattoria"])),
        ]
    );
    assert!(plan.summary().contains(&"SOP `reservations` removed → 1 agent: luigis-trattoria".to_string()));
    assert!(plan.summary().contains(&"instruction `pizza-context` added → 1 agent: sakura-sushi".to_string()));
}

#[test]
fn plan_attributes_group_changes_to_sopc_yaml() {
    let r = Repo::new();
    r.append("sopc.yaml", "\ngroups:\n  end:\n    - reservations\n    - closing\n");
    r.edit("agents/sakura-sushi.yaml", "  - reservations\n  - closing\n", "  - end\n");
    let before = snapshot(&build(r.root()));
    r.edit("sopc.yaml", "    - reservations\n    - closing\n", "    - closing\n    - reservations\n");
    let plan = make_plan(&before, &snapshot(&build(r.root())));
    assert_eq!(groups(&plan), [("workspace:sopc.yaml".into(), "edited", strings(&["sakura-sushi"]))]);
}

#[test]
fn plan_attributes_default_variable_changes_to_workspace() {
    let r = Repo::new();
    let before = snapshot(&build(r.root()));
    r.edit("sopc.yaml", "the manager on duty", "the shift lead");
    let plan = make_plan(&before, &snapshot(&build(r.root())));
    assert_eq!(
        groups(&plan),
        [("workspace:sopc.yaml".into(), "edited", strings(&["luigis-trattoria", "tonys-pizza"]))]
    );
}

#[test]
fn plan_new_and_removed_agents() {
    let r = Repo::new();
    let before = snapshot(&build(r.root()));
    std::fs::remove_file(r.path("agents/sakura-sushi.yaml")).unwrap();
    std::fs::copy(r.path("agents/luigis-trattoria.yaml"), r.path("agents/luigis-brooklyn.yaml")).unwrap();
    r.edit("agents/luigis-brooklyn.yaml", "livekit: luigis-trattoria", "livekit: luigis-brooklyn");
    let plan = make_plan(&before, &snapshot(&build(r.root())));
    let got: Vec<String> = plan.changes.iter().map(|c| format!("{} {}", c.agent, c.status)).collect();
    assert_eq!(got, ["luigis-brooklyn added", "sakura-sushi removed"]);
    assert!(plan.changes[0].diff.contains("@@ -0,0 +1,"));
}

#[test]
fn no_change_means_empty_plan() {
    let expected = read_snapshot(&fixture().join("expected")).unwrap();
    assert!(make_plan(&snapshot(&build(&sops())), &expected).is_empty());
}

#[test]
fn affected_picks_agents_whose_prompt_changed_with_the_blocks_that_changed_it() {
    let r = Repo::new();
    let base = build(r.root());
    r.edit("procedures/reservations.yaml", "Never double-book a table", "Never double-book or overbook a table");
    r.edit("instructions/pizza-context.md", "12\" and 16\"", "10\", 12\" and 16\"");
    let result = affected(&build(r.root()), Some(&base), &[], false).unwrap();
    assert!(!result.all);
    let got: BTreeMap<&str, (&str, Vec<String>, Vec<String>)> =
        result.agents.iter().map(|a| (a.id.as_str(), (a.reason, a.changed.clone(), a.changed_sops.clone()))).collect();
    let want: BTreeMap<&str, (&str, Vec<String>, Vec<String>)> = [
        ("luigis-trattoria", ("changed", strings(&["sop:reservations"]), strings(&["reservations"]))),
        ("sakura-sushi", ("changed", strings(&["sop:reservations"]), strings(&["reservations"]))),
        ("tonys-pizza", ("changed", strings(&["instruction:pizza-context"]), vec![])),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
}

#[test]
fn affected_when_nothing_changed() {
    let head = build(&sops());
    assert!(affected(&head, Some(&head), &[], false).unwrap().agents.is_empty());
    let everyone = affected(&head, Some(&head), &[], true).unwrap();
    assert!(everyone.all);
    assert_eq!(everyone.ids(), head.agents.keys().cloned().collect::<Vec<_>>());
}

#[test]
fn affected_without_a_base_is_every_agent() {
    let result = affected(&build(&sops()), None, &[], false).unwrap();
    assert!(result.all && result.agents.len() == 3);
}

#[test]
fn affected_requested_by_sopc_id_or_platform_id() {
    let r = Repo::new();
    r.edit("agents/luigis-trattoria.yaml", "livekit: luigis-trattoria", "vapi: asst_9f3e");
    let head = build(r.root());
    let result =
        affected(&head, Some(&head), &strings(&["asst_9f3e", "sakura-sushi", "vapi:asst_9f3e"]), false).unwrap();
    assert_eq!(result.ids(), ["luigis-trattoria", "sakura-sushi"]);
    assert_eq!(result.agents[0].platform_id, "asst_9f3e");
    assert!(result.github_outputs().contains(&("platform_ids", "asst_9f3e sakura-sushi".to_string())));
    let err = affected(&head, Some(&head), &strings(&["la-casa"]), false).err().unwrap();
    assert!(err.to_string().contains("unknown agent(s): la-casa"));
}

// --- analysis --------------------------------------------------------------------------------------

fn read_dir(dir: &Path, suffix: &str) -> BTreeMap<String, String> {
    let names = file_names(dir).into_iter().filter(|n| n.ends_with(suffix));
    names.map(|n| (n.trim_end_matches(suffix).to_string(), read(&dir.join(&n)))).collect()
}

fn originals() -> BTreeMap<String, String> {
    read_dir(&fixture().join("originals"), ".md")
}

#[test]
fn overlap_finds_text_every_prompt_shares() {
    let result = overlap(&originals(), 0.75);
    let everyone: Vec<&str> = result.shared.iter().filter(|s| s.agents().len() == 3).map(|s| s.text.as_str()).collect();
    assert!(everyone.contains(&"Speak warmly and briefly."));
    assert!(everyone.contains(&"Before hanging up, repeat the order total and the pickup or delivery time."));
}

#[test]
fn overlap_finds_placeholder_candidates_and_drift() {
    let result = overlap(&originals(), 0.75);
    let find = |word: &str| {
        &result
            .near_copies
            .iter()
            .find(|nc| nc.variants.values().next().unwrap().text.contains(word))
            .unwrap()
            .differing
    };
    let host: Vec<(&str, &str)> = find("phone host").iter().map(|(a, d)| (a.as_str(), d.as_str())).collect();
    assert_eq!(
        host,
        [("luigis-trattoria", "Luigi's Trattoria"), ("sakura-sushi", "Sakura Sushi"), ("tonys-pizza", "Tony's Pizza")]
    );
    assert_eq!(find("upsell")["luigis-trattoria"], "twice");
    assert!(!result.shared.iter().any(|s| s.text.contains("upsell")), "drifted text is reported once, as a near-copy");
}

#[test]
fn unit_spans_point_at_the_original_text() {
    let text = "# Host\n\nYou are the host for Café Ünder.  Speak warmly and briefly.\r\n- Goal: Ask one question at a time.\n1. Never upsell more than once.\n";
    let spans = unit_spans(text);
    let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, units(text));
    assert_eq!(
        texts,
        [
            "You are the host for Café Ünder.",
            "Speak warmly and briefly.",
            "Ask one question at a time.",
            "Never upsell more than once."
        ]
    );
    let chars: Vec<char> = text.chars().collect();
    for s in &spans {
        assert_eq!(chars[s.start..s.end].iter().collect::<String>(), s.text, "offsets are in characters");
    }
}

#[test]
fn overlap_json_classifies_near_copies_and_locates_them() {
    let prompts: BTreeMap<String, String> = [
        ("bistro", "You are the host at Bistro Blue, the corner restaurant on Main Street.\nGuests must show ID for wine.\nWe close at 10pm."),
        ("diner", "You are the host at Dina's Diner, the corner restaurant on Main Street.\nGuests may show ID for wine.\nWe close at 10pm."),
    ]
    .into_iter()
    .map(|(a, p)| (a.to_string(), p.to_string()))
    .collect();
    let json = overlap(&prompts, 0.75).to_json();
    assert_eq!(json["shared"][0]["text"], "We close at 10pm.");
    let near = json["near_copies"].as_array().unwrap();
    assert_eq!(near.len(), 2);
    assert_eq!(near[0]["kind"], "value");
    assert_eq!(near[0]["suggested_variable"], "host");
    assert_eq!(near[0]["differing_tokens"]["diner"], "Dina's Diner");
    assert_eq!(near[1]["kind"], "rule_word", "must vs may is drift, not a value");
    assert_eq!(near[1]["suggested_variable"], serde_json::Value::Null);
    assert_eq!(near[1]["agents_by_variant"]["Guests may show ID for wine."], serde_json::json!(["diner"]));
    for nc in near {
        for o in nc["occurrences"].as_array().unwrap() {
            let prompt: Vec<char> = prompts[o["agent"].as_str().unwrap()].chars().collect();
            let at = |v: &serde_json::Value| -> String {
                prompt[v["start"].as_u64().unwrap() as usize..v["end"].as_u64().unwrap() as usize].iter().collect()
            };
            assert_eq!(at(o), o["text"]);
            for d in o["differing"].as_array().unwrap() {
                assert_eq!(at(d), d["text"]);
            }
        }
    }
}

/// The kind and suggested variable of the one near-copy between two one-sentence prompts.
fn near_copy_kind(a: &str, b: &str) -> (String, serde_json::Value) {
    let prompts: BTreeMap<String, String> =
        [("bistro".to_string(), a.to_string()), ("diner".to_string(), b.to_string())].into_iter().collect();
    let json = overlap(&prompts, 0.75).to_json();
    let near = json["near_copies"].as_array().unwrap();
    assert_eq!(near.len(), 1, "{a} / {b}: {json}");
    (near[0]["kind"].as_str().unwrap().to_string(), near[0]["suggested_variable"].clone())
}

#[test]
fn identifiers_are_values_and_counts_are_rule_words() {
    let value = |name: &str| ("value".to_string(), serde_json::json!(name));
    let rule = ("rule_word".to_string(), serde_json::Value::Null);
    for (a, b, want) in [
        (
            "Our phone number is 555-0142 for catering orders.",
            "Our phone number is 555-0199 for catering orders.",
            value("phone_number"),
        ),
        (
            "Our phone number is (212) 555 0142 for catering orders.",
            "Our phone number is (718) 555 0199 for catering orders.",
            value("phone_number"),
        ),
        (
            "The large margherita pizza costs $12.99 before tax.",
            "The large margherita pizza costs $13.49 before tax.",
            value("pizza_costs"),
        ),
        (
            "The kitchen closes at 10pm on weekdays and weekends.",
            "The kitchen closes at 11pm on weekdays and weekends.",
            value("kitchen_closes"),
        ),
        (
            "Pickups are at 12 Mulberry Street, next to the bakery.",
            "Pickups are at 48 Mulberry Street, next to the bakery.",
            value("pickups"),
        ),
        (
            "Menus are online at bistro.example/menu for every guest.",
            "Menus are online at diner.example/menu for every guest.",
            value("menus_online"),
        ),
        (
            "Never upsell more than once per call to any guest.",
            "Never upsell more than twice per call to any guest.",
            rule.clone(),
        ),
        (
            "Offer dessert up to 2 times during each call please.",
            "Offer dessert up to 3 times during each call please.",
            rule.clone(),
        ),
        (
            "Offer dessert up to 2 during each call to every guest.",
            "Offer dessert up to 3 during each call to every guest.",
            rule.clone(),
        ),
        (
            "Confirm the order within 5 minutes of the guest calling.",
            "Confirm the order within 10 minutes of the guest calling.",
            rule.clone(),
        ),
        (
            "Guests must show ID before ordering wine at the table.",
            "Guests may show ID before ordering wine at the table.",
            rule.clone(),
        ),
    ] {
        assert_eq!(near_copy_kind(a, b), want, "{a} / {b}");
    }
}

#[test]
fn compare_json_reports_scores_and_reworded_candidates() {
    let build = build(&sops());
    let json = compare_json(&compare(&build, &originals(), 0.9), &build);
    assert_eq!(json["summary"], serde_json::json!({"ok": false, "agents": 3, "failed": 2}));
    let luigis = &json["agents"][0];
    assert_eq!(
        (luigis["agent"].as_str(), luigis["sentences_total"].as_u64(), luigis["kept"].as_u64()),
        (Some("luigis-trattoria"), Some(8), Some(7))
    );
    assert_eq!(luigis["score"], 0.875);
    assert_eq!(luigis["changed"][0]["changed_words"], serde_json::json!({"original": "twice", "compiled": "once"}));
    let tonys = &json["agents"][2];
    let candidates = tonys["reworded"][0]["compiled_candidates"].as_array().unwrap();
    assert!(candidates.iter().any(|c| c == "Name the specific allergen back to the customer"), "{candidates:?}");
}

#[test]
fn compare_passes_when_nothing_was_lost() {
    let originals = read_dir(&fixture().join("expected"), ".prompt.md");
    for r in compare(&build(&sops()), &originals, 0.9) {
        assert!(r.ok() && r.coverage() == 1.0 && r.added.is_empty(), "{r:?}");
    }
}

#[test]
fn compare_reports_changed_missing_and_reworded() {
    let mut originals = originals();
    let sakura = originals.get_mut("sakura-sushi").unwrap();
    *sakura = format!("{sakura}\nGift cards can be bought at the counter on weekends.\n")
        .replace("Ask one question at a time.", "Ask up to two questions at a time.");
    let results: BTreeMap<String, _> =
        compare(&build(&sops()), &originals, 0.9).into_iter().map(|r| (r.agent.clone(), r)).collect();
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        results["luigis-trattoria"].changed,
        [pair("Never upsell more than twice per call.", "Never upsell more than once per call.")]
    );
    assert_eq!(results["sakura-sushi"].missing, ["Gift cards can be bought at the counter on weekends."]);
    assert_eq!(
        results["sakura-sushi"].changed,
        [pair("Ask up to two questions at a time.", "Ask one question at a time.")]
    );
    assert!(results["tonys-pizza"].reworded.iter().any(|(u, _)| u.starts_with("ALLERGIES:")));
    for r in results.values() {
        assert!(!r.ok() || (r.agent == "tonys-pizza" && r.changed.is_empty()), "{} should not be ok", r.agent);
        assert!(!r.added.iter().any(|u| u.contains("tool.")), "sopc's own tool lines are not 'added'");
    }
}

#[test]
fn check_is_clean_on_the_fixture() {
    assert!(lint(&ws(&sops())).is_empty());
}

#[test]
fn check_finds_mechanical_conflicts() {
    let r = Repo::new();
    r.edit(
        "agents/tonys-pizza.yaml",
        "Pickup only after 10pm. Cash and card.",
        "Pickup only after 11pm. Cash and card. Speak warmly and briefly. Always confirm the delivery address.",
    );
    r.edit(
        "agents/tonys-pizza.yaml",
        "  menu_allergen_link: tonys.com/allergens",
        "  menu_allergen_link: tonys.com/allergens\n  old_phone: 555-0100",
    );
    r.append("instructions/pizza-context.md", " Pickup only after 10pm.\n");
    r.edit(
        "procedures/delivery-handling.yaml",
        "procedureSteps:",
        "forbiddenActions:\n  - Never confirm the delivery address\nprocedureSteps:",
    );
    let findings = lint(&ws(r.root()));
    let by_code: BTreeMap<&str, &crate::analyze::Finding> = findings.iter().map(|f| (f.code, f)).collect();
    assert_eq!(
        by_code.keys().copied().collect::<Vec<_>>(),
        ["duplicate_text", "negation_conflict", "numeric_conflict", "unused_variable"]
    );
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        by_code["numeric_conflict"].sources,
        [
            pair("agent `tonys-pizza`", "Pickup only after 11pm."),
            pair("instruction `pizza-context`", "Pickup only after 10pm.")
        ]
    );
    assert_eq!(by_code["negation_conflict"].agents, ["tonys-pizza"]);
    assert_eq!(by_code["duplicate_text"].sources[0].0, "agent `tonys-pizza`");
    assert_eq!(by_code["duplicate_text"].sources[1].0, "instruction `brand-voice`");
}

#[test]
fn check_reports_a_shared_conflict_once_for_all_agents() {
    let r = Repo::new();
    r.append(
        "instructions/closing.md",
        " Before hanging up, never repeat the order total and the pickup or delivery time.\n",
    );
    let findings = lint(&ws(r.root()));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, "negation_conflict");
    assert_eq!(findings[0].agents, ["luigis-trattoria", "sakura-sushi", "tonys-pizza"]);
}

#[test]
fn check_finds_a_number_conflict_inside_a_longer_sentence() {
    let r = Repo::new();
    r.edit(
        "agents/tonys-pizza.yaml",
        "Pickup only after 10pm. Cash and card.",
        "Pickup and delivery until 11pm. Delivery until 10pm on Sundays. Cash and card.",
    );
    assert!(lint(&ws(r.root())).is_empty(), "different statements: no conflict");
    r.edit("agents/tonys-pizza.yaml", "Delivery until 10pm on Sundays.", "Delivery until 10pm.");
    let findings = lint(&ws(r.root()));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, "numeric_conflict");
}

/// Deterministic sentences: random words, some numbers, and near-copies of earlier ones with a
/// word swapped, a number changed, a negation added or removed, or text around them.
fn generated_sentences(n: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "customer", "order", "table", "menu", "item", "delivery", "address", "card", "refund", "allergy", "guest",
        "booking", "time", "party", "manager", "staff", "kitchen", "drink", "coupon", "phone", "name", "receipt",
        "pickup", "confirm", "check", "ask", "tell", "offer", "repeat", "transfer", "the", "to", "and", "always", "pm",
        "minutes", "two", "ten", "once",
    ];
    const OPENERS: &[&str] = &["Always", "Never", "Do not", "Please", "Only", "Don't", "You cannot", "Make sure to"];
    let mut seed: u64 = 7;
    let mut next = move |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n
    };
    let mut out: Vec<String> = vec![];
    for _ in 0..n {
        let s = if !out.is_empty() && next(3) == 0 {
            let base = out[next(out.len())].trim_end_matches('.').to_string();
            let mut ws: Vec<String> = base.split(' ').map(String::from).collect();
            match next(6) {
                0 => {
                    let i = next(ws.len());
                    ws[i] = WORDS[next(WORDS.len())].to_string();
                }
                1 => ws.push(next(90).to_string()),
                2 => ws.insert(1, "never".into()),
                3 => ws.retain(|w| !["Never", "not", "never", "Don't"].contains(&w.as_str())),
                4 => ws.extend(["and", "then", "ask"].map(String::from)),
                _ => ws.iter_mut().filter(|w| w.chars().all(|c| c.is_ascii_digit())).for_each(|w| *w = "11:30".into()),
            }
            ws.join(" ")
        } else {
            let mut ws = vec![OPENERS[next(OPENERS.len())].to_string()];
            ws.extend((0..3 + next(12)).map(|_| WORDS[next(WORDS.len())].to_string()));
            if next(3) == 0 {
                ws.insert(1 + next(ws.len() - 1), next(40).to_string());
            }
            ws.join(" ")
        };
        out.push(s + ".");
    }
    out
}

#[test]
fn lint_conflicts_match_the_direct_definition() {
    let sentences = generated_sentences(200);
    let mut codes: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, a) in sentences.iter().enumerate() {
        for b in &sentences[i + 1..] {
            let want = crate::analyze::conflict(a, b);
            assert_eq!(crate::analyze::fast_conflict(a, b), want, "{a:?} vs {b:?}");
            *codes.entry(want.unwrap_or("none")).or_default() += 1;
        }
    }
    // Every code is exercised.
    for code in ["duplicate_text", "numeric_conflict", "negation_conflict", "near_duplicate", "none"] {
        assert!(codes.get(code).is_some_and(|n| *n > 3), "{code}: {codes:?}");
    }
}

#[test]
fn lint_matches_the_naive_lint_on_a_generated_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sentences = generated_sentences(150);
    let mut chunks = sentences.chunks(4);
    std::fs::write(root.join("sopc.yaml"), "version: 1\n").unwrap();
    for sub in ["instructions", "procedures", "agents"] {
        std::fs::create_dir(root.join(sub)).unwrap();
    }
    for i in 0..25 {
        std::fs::write(root.join(format!("instructions/i{i}.md")), chunks.next().unwrap().join(" ") + "\n").unwrap();
    }
    for i in 0..10 {
        let steps: Vec<String> = chunks.next().unwrap().iter().map(|s| s.trim_end_matches('.').to_string()).collect();
        let body = format!(
            "# P{i}\n\n**Goal:** Done well.\n**When:** Asked.\n\n## Steps\n1. {}\n\n## Never\n- {}\n",
            steps[..3].join("\n1. "),
            steps[3]
        );
        std::fs::write(root.join(format!("procedures/p{i}.md")), body).unwrap();
    }
    for a in 0..8 {
        let blocks: Vec<String> = (0..25)
            .filter(|i| (i * 7 + a * 3) % 5 < 3)
            .map(|i| format!("  - i{i}"))
            .chain((0..10).filter(|i| (i + a) % 3 != 0).map(|i| format!("  - p{i}")))
            .collect();
        let text = format!("livekit: a{a}\ncontext: |\n  {}\nblocks:\n{}\n", sentences[a * 5], blocks.join("\n"));
        std::fs::write(root.join(format!("agents/a{a:02}.yaml")), text).unwrap();
    }
    let ws = ws(root);
    let json = |fs: Vec<crate::analyze::Finding>| fs.iter().map(|f| f.to_json()).collect::<Vec<_>>();
    let (fast, naive) = (json(lint(&ws)), json(crate::analyze::lint_naive(&ws)));
    assert!(fast.len() > 20, "{}", fast.len());
    assert_eq!(fast, naive);
}

#[test]
fn units_split_sentences_and_drop_labels() {
    let got = units("## Heading\nSteps:\n1. Ask the caller. Then check! Okay? yes no\n- Goal: Keep it short. \"Quoted start\" here\n  two words\n");
    assert_eq!(got, ["Ask the caller.", "Okay? yes no", "Keep it short.", "\"Quoted start\" here"]);
}

#[test]
fn ascii_json_escapes_like_python() {
    let v = serde_json::json!({"a": "é😀\u{7f}"});
    assert_eq!(pretty_json(&v, true), "{\n  \"a\": \"\\u00e9\\ud83d\\ude00\\u007f\"\n}");
}

// --- spec/*.schema.json ------------------------------------------------------------------------------

fn schema(name: &str) -> serde_json::Value {
    serde_json::from_str(&read(&repo_root().join("spec").join(name))).unwrap()
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn list(v: &serde_json::Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()).unwrap_or_default()
}

#[test]
fn fields_match_the_published_schemas() {
    // (schema, fields in canonical order, required fields a file must set)
    for (file, fields, required) in [
        ("instruction.schema.json", &INSTRUCTION_FIELDS[..INSTRUCTION_FIELDS.len() - 1], &[][..]), // "text" is the body
        ("sop.schema.json", SOP_FIELDS, SOP_REQUIRED),
        ("agent.schema.json", AGENT_FIELDS, &[][..]),
        ("sopc.schema.json", CONFIG_FIELDS, &[][..]),
    ] {
        let s = schema(file);
        assert_eq!(keys(&s["properties"]), fields, "{file} properties");
        assert_eq!(list(&s["required"]), required, "{file} required");
    }
    let sop = schema("sop.schema.json");
    assert_eq!(list(&sop["properties"]["delivery"]["enum"]), DELIVERIES);
    assert_eq!(keys(&sop["$defs"]["Step"]["properties"]), STEP_FIELDS);
    assert_eq!(list(&sop["$defs"]["Step"]["required"]), STEP_REQUIRED);
    assert_eq!(schema("sopc.schema.json")["properties"]["version"]["const"], 1);
    let agent = schema("agent.schema.json");
    assert!(PLATFORMS.iter().all(|p| agent["properties"].get(*p).is_some()));
}

// --- Markdown SOPs, fmt and convert -------------------------------------------------------------------

use crate::sopfile::{rewrite, Kind};
use crate::workspace::parse_sop_file;

fn md_fixture() -> String {
    read(&repo_root().join("tests/fixtures/markdown/allergen-check.md"))
}

/// (code, line) of each problem in a Markdown SOP.
fn md_problems(text: &str) -> Vec<(&'static str, usize)> {
    let issues = parse_sop_file("procedures/x.md", text).err().unwrap_or_default();
    let line = |m: &str| m.strip_prefix("line ").and_then(|r| r.split(':').next()?.parse().ok()).unwrap_or(0);
    issues.iter().map(|i| (i.code, line(&i.message))).collect()
}

const MD_OK: &str = "# Name\n\n**Goal:** g\n\n## Steps\n1. Do it\n";

#[test]
fn markdown_sop_reads_into_the_same_sop_as_yaml() {
    let (md, _) = parse_sop_file("procedures/allergen-check.md", &md_fixture()).unwrap();
    let yaml_text = read(&sops().join("procedures/allergen-check.yaml"));
    let (yaml, _) = parse_sop_file("procedures/allergen-check.yaml", &yaml_text).unwrap();
    assert_eq!(md.description, "Customer leaves knowing whether their order is safe for their allergy.");
    assert_eq!(md.procedure_steps[2].tool.as_deref(), Some("lookup_allergens"));
    assert!(md.procedure_steps[2].required && md.procedure_steps[0].plain);
    // Only the guidance's trailing newline (from the YAML `|` block) differs.
    let yaml = Sop { guidance: yaml.guidance.trim_end().to_string(), file: md.file.clone(), ..yaml };
    assert_eq!(md.canonical_json(), yaml.canonical_json());
}

#[test]
fn markdown_sop_renders_the_same_prompts_and_tool_json() {
    let r = Repo::new();
    std::fs::remove_file(r.path("procedures/allergen-check.yaml")).unwrap();
    std::fs::write(r.path("procedures/allergen-check.md"), md_fixture()).unwrap();
    let out = tempfile::tempdir().unwrap();
    write_build(&build(r.root()), out.path(), false).unwrap();
    for (name, text) in read_dir(&fixture().join("expected"), "") {
        if name != "lock.json" {
            assert_eq!(read(&out.path().join(&name)), text, "{name}");
        }
    }
}

#[test]
fn markdown_fields_guidance_and_any_section_order() {
    let text = "---\ndelivery: auto\n---\n# Name\n**When:** a\nb\n\nOne.\nTwo.\n\nThree.\n\n## Warning signs\n- w\n## Steps\n1. s `required`\n";
    let (sop, warnings) = parse_sop_file("procedures/x.md", text).unwrap();
    assert!(warnings.is_empty());
    assert_eq!((sop.scope.as_str(), sop.guidance.as_str()), ("a b", "One.\nTwo.\n\nThree."));
    assert_eq!((sop.delivery.as_str(), sop.warning_signs[0].text.as_str()), ("auto", "w"));
    assert!(sop.procedure_steps[0].required && sop.procedure_steps[0].tool.is_none());
}

#[test]
fn markdown_formatting_checks_report_code_and_line() {
    for (text, want) in [
        ("---\nname: X\ndelivery: auto\n---\n# Name\n## Steps\n1. a\n", ("md_settings_field", 2)),
        ("---\ndelivery: auto\nprocedureSteps: [a]\n---\n# Name\n## Steps\n1. a\n", ("md_settings_field", 3)),
        ("---\nlocked: true\n---\n# Name\n## Steps\n1. a\n", ("removed_field", 0)),
        ("---\nagents: \"*\"\n---\n# Name\n## Steps\n1. a\n", ("old_format", 0)),
        ("Intro\n# Name\n## Steps\n1. a\n", ("md_text_before_name", 1)),
        ("# Name\n## Steps\n1. a\n# Other\n", ("md_extra_name", 4)),
        ("# Name\n## Steps\n1. a\n## Notes\n- b\n", ("md_unknown_section", 4)),
        ("# Name\n## steps\n1. a\n## Steps\n1. a\n", ("md_unknown_section", 2)),
        ("# Name\n## Steps\n1. a\n## Steps\n1. b\n", ("md_duplicate_section", 4)),
        ("# Name\n### Detail\n## Steps\n1. a\n", ("md_heading_level", 2)),
        ("# Name\n**Objective:** o\n## Steps\n1. a\n", ("md_unknown_field", 2)),
        ("# Name\n**Goal**: o\n## Steps\n1. a\n", ("md_unknown_field", 2)),
        ("# Name\n**Goal:** a\n\n**Goal:** b\n## Steps\n1. a\n", ("md_duplicate_field", 4)),
        ("# Name\n## Steps\n1. a\nloose text\n", ("md_text_in_section", 4)),
        ("# Name\n## Steps\n1. a\n* b\n", ("md_text_in_section", 4)),
        ("# Name\n## Steps\n1. a `tool:`\n", ("md_bad_annotation", 3)),
        ("# Name\n## Steps\n1. a `tools: x`\n", ("md_bad_annotation", 3)),
        ("# Name\n## Steps\n1. a `tool: two words`\n", ("md_bad_annotation", 3)),
        ("# Name\n## Steps\n1. a `tool: x` `tool: y`\n", ("md_bad_annotation", 3)),
        ("# Name\n## Steps\n1. a\n## Never\n- b `required`\n", ("md_required_outside_steps", 5)),
        ("# Name\n## Steps\n1. a\n2. `tool: x`\n", ("empty_step", 4)),
        ("# Name\n**Goal:** g\n", ("missing_steps", 1)),
        ("# Name\n\n## Steps\n\n## Never\n- n\n", ("missing_steps", 3)),
    ] {
        assert_eq!(md_problems(text), [want], "{text}");
    }
    assert_eq!(md_problems("\n\n"), [("md_missing_name", 1)]);
    assert_eq!(md_problems("**Goal:** g\n## Steps\n1. a\n"), [("md_text_before_name", 1), ("md_missing_name", 1)]);
    assert_eq!(
        md_problems("---\ndelivery: auto\n---\n## Steps\n1. a\n"),
        [("md_text_before_name", 4), ("md_missing_name", 4)]
    );
    assert!(md_problems(MD_OK).is_empty());
    // Ordinary code at the end of an item is text, and indented lines continue it.
    let (sop, _) = parse_sop_file("procedures/x.md", "# N\n## Steps\n1. Say `hello`\n  and wait\n").unwrap();
    assert_eq!(sop.procedure_steps[0].text, "Say `hello` and wait");
}

#[test]
fn empty_never_or_warning_section_is_a_warning() {
    let (_, warnings) = parse_sop_file("procedures/x.md", &format!("{MD_OK}\n## Never\n")).unwrap();
    assert_eq!(codes(&warnings), ["md_empty_section"]);
    assert_eq!(warnings[0].message, "line 8: `## Never` has no items; add some or remove the heading");
    let r = Repo::new();
    std::fs::write(r.path("procedures/extra.md"), format!("{MD_OK}\n## Warning signs\n")).unwrap();
    assert_eq!(codes(&validate(&ws(r.root()))), ["md_empty_section", "unused_block"]);
}

#[test]
fn steps_are_required_in_yaml_too() {
    let r = Repo::new();
    r.edit("procedures/reservations.yaml", "procedureSteps:", "procedureSteps: []\nx:");
    let issues = load_issues(r.root());
    assert_eq!(codes(&issues), ["missing_steps"]);
    assert_eq!(issues[0].path, "procedures/reservations.yaml");
    assert!(issues[0].message.starts_with("line 4: "), "{}", issues[0].message);
    let r = Repo::new();
    std::fs::write(r.path("procedures/reservations.yaml"), "name: Reservations\ndescription: d\n").unwrap();
    assert_eq!(codes(&load_issues(r.root())), ["missing_steps"]);
}

#[test]
fn same_sop_as_markdown_and_yaml_is_an_error() {
    let r = Repo::new();
    std::fs::write(r.path("procedures/reservations.md"), MD_OK).unwrap();
    assert_eq!(codes(&load_issues(r.root())), ["duplicate_file"]);
}

#[test]
fn markdown_sop_paths_are_used_in_issues() {
    let r = Repo::new();
    std::fs::write(
        r.path("procedures/extra.md"),
        format!("---\ndelivery: auto\n---\n{}", MD_OK.replace("**Goal:** g\n", "")),
    )
    .unwrap();
    let issues = validate(&ws(r.root()));
    assert_eq!(codes(&issues), ["missing_goal", "unused_block"]);
    assert!(issues.iter().all(|i| i.path == "procedures/extra.md"));
}

#[test]
fn fmt_is_canonical_and_idempotent() {
    let (_, once) = rewrite("procedures/allergen-check.md", &md_fixture(), Kind::Markdown).unwrap();
    assert!(
        once.contains("## Steps\n1. Ask")
            && once.contains("2. Name")
            && once.contains("`tool: lookup_allergens` `required`")
    );
    assert!(once.find("## Steps").unwrap() < once.find("## Never").unwrap());
    assert!(once.starts_with("---\n# The allergen SOP"), "front matter is kept as written");
    let (_, twice) = rewrite("procedures/allergen-check.md", &once, Kind::Markdown).unwrap();
    assert_eq!(once, twice);
    // Every fixture YAML file is already canonical.
    for (name, text) in read_dir(&sops().join("procedures"), ".yaml") {
        let path = format!("procedures/{name}.yaml");
        assert_eq!(rewrite(&path, &text, Kind::Yaml).unwrap().1, text, "{name}");
    }
}

#[test]
fn fmt_keeps_yaml_values_exactly() {
    let text = "# header comment\n\nid: x\ndelivery: prompt   # dropped\nname: 'X'\nguidance: |+\n  keep\n\n   indented\n\nprocedureSteps:\n- text: plain object\n- \"Say: hi\"\n- |-\n  two\n  lines\n- text: t\n  tool: ''\nscope: >\n  folded\n  text\n";
    let (sop, _) = parse_sop_file("procedures/x.yaml", text).unwrap();
    let (_, out) = rewrite("procedures/x.yaml", text, Kind::Yaml).unwrap();
    assert!(out.starts_with("# header comment\n\nname: X\nscope: |\n"), "{out}");
    let (back, _) = parse_sop_file("procedures/x.yaml", &out).unwrap();
    assert_eq!(back.canonical_json(), sop.canonical_json());
    assert_eq!(rewrite("procedures/x.yaml", &out, Kind::Yaml).unwrap().1, out);
}

#[test]
fn convert_refuses_what_markdown_cannot_hold() {
    let text = "name: X\ndescription: |\n  two\n  lines\nprocedureSteps: [a]\n";
    let err = rewrite("procedures/x.yaml", text, Kind::Markdown).unwrap_err();
    assert_eq!(codes(&err), ["convert_failed"]);
    assert!(err[0].message.contains("description would change"), "{}", err[0].message);
    for step in ["\"Type `required`\"", "|-\n    two\n    lines"] {
        let text = format!("name: X\nprocedureSteps:\n  - {step}\n");
        assert_eq!(codes(&rewrite("procedures/x.yaml", &text, Kind::Markdown).unwrap_err()), ["convert_failed"]);
    }
}

#[test]
fn convert_round_trip_keeps_yaml_and_comments() {
    for (name, text) in read_dir(&sops().join("procedures"), ".yaml") {
        let path = format!("procedures/{name}.yaml");
        let (md_path, md) = rewrite(&path, &text, Kind::Markdown).unwrap();
        assert_eq!(rewrite(&md_path, &md, Kind::Yaml).unwrap(), (path, text), "{name}");
    }
    let yaml = "# yaml-language-server: $schema=x\n#\n# Why this SOP exists.\n\nname: X\nprocedureSteps: [a]\n";
    let (_, md) = rewrite("procedures/x.yaml", yaml, Kind::Markdown).unwrap();
    assert_eq!(md, "---\n# Why this SOP exists.\n---\n# X\n\n## Steps\n1. a\n");
    assert_eq!(
        rewrite("procedures/x.md", &md, Kind::Yaml).unwrap().1,
        "# Why this SOP exists.\n\nname: X\nprocedureSteps:\n  - a\n"
    );
}

// --- migrate -----------------------------------------------------------------------------------------

use crate::migrate;
use crate::workspace::{load_files, read_files};

fn old_fixture(name: &str) -> PathBuf {
    repo_root().join("tests/fixtures/old-format").join(name)
}

fn migrated(name: &str) -> migrate::Migration {
    let files = read_files(&old_fixture(name).join("sops")).unwrap();
    assert!(migrate::is_old(&files));
    migrate::migrate(&files).unwrap()
}

#[test]
fn migrate_reproduces_the_fixture_exactly() {
    let m = migrated("restaurants");
    assert_eq!(m.files, read_files(&sops()).unwrap());
    assert!(!m.removed_fields_only);
    assert_eq!(migrate::verify(&m.files, &m.expected).unwrap(), 3);
    assert!(!migrate::is_old(&m.files));
}

#[test]
fn migrate_keeps_every_prompt_except_the_context_moving_to_the_top() {
    for name in ["restaurants", "livekit-restaurant"] {
        let dir = old_fixture(name);
        let m = migrated(name);
        let build = render_workspace(&load_files(&m.files).unwrap()).unwrap();
        assert_eq!(build.agents.len(), if name == "restaurants" { 3 } else { 4 });
        for (id, r) in &build.agents {
            // The old prompts, as the previous release built them.
            let old = read(&dir.join(format!("expected/{id}.prompt.md")));
            let context = r.agent.context.trim();
            assert!(old.contains(&format!("\n\n{context}\n\n")), "{name}/{id}");
            let want = format!("{context}\n\n{}", old.replacen(&format!("\n\n{context}"), "", 1));
            assert_eq!(r.prompt, want, "{name}/{id}");
            let tool = dir.join(format!("expected/{id}.tool.json"));
            let payload = serde_json::Value::Object(r.tool_payload.clone());
            if tool.exists() {
                assert_eq!(pretty_json(&payload, false) + "\n", read(&tool), "{name}/{id}");
            } else {
                assert!(r.tool_payload.is_empty(), "{name}/{id}");
            }
        }
    }
}

#[test]
fn migrate_writes_context_then_blocks_as_a_bullet_list() {
    let m = migrated("livekit-restaurant");
    assert_eq!(
        m.files["agents/sakura-sushi.yaml"],
        "# yaml-language-server: $schema=../../../../../../spec/agent.schema.json

livekit: sakura-sushi
context: |
  Sakura is an omakase and sushi counter in Manhattan. Reservations strongly recommended. No delivery.
blocks:
  - restaurant-host
  - brand-voice
  - allergen-check
  - reservations
  - closing

variables:
  restaurant_name: Sakura Sushi
  menu_allergen_link: sakurasushi.nyc/allergens
  staff_transfer: the head chef # overrides the default in sopc.yaml
"
    );
    assert!(!m.files.keys().any(|p| p.starts_with("bases/")));
    assert_eq!(m.removes.len(), 5);
    // Front matter that only held old fields goes; comments that stay are kept.
    assert!(m.files["instructions/closing.md"].starts_with("Before hanging up"));
    assert!(m.files["instructions/restaurant-host.md"].starts_with("---\n# A base is prompt text"));
    // Comments right above a removed field go with it, and are listed.
    assert!(m.files["procedures/allergen-check.md"].starts_with("---\ndelivery: prompt  # prompt (default)"));
    let lost = m.lost_comments.iter().find(|(p, _)| p == "procedures/allergen-check.md").unwrap();
    assert_eq!(lost.1.len(), 3);
    let lost = m.lost_comments.iter().find(|(p, _)| p == "agents/sakura-sushi.yaml").unwrap();
    assert_eq!(lost.1, ["line 6: # Opt out of blocks that target every agent. Locked blocks can't be excluded."]);
}

#[test]
fn migrate_drops_every_lock() {
    let m = migrated("livekit-restaurant");
    // la-casita lists the Spanish voice instead of the English one.
    let casita = &m.files["agents/la-casita.yaml"];
    assert!(casita.contains("  - brand-voice-es\n") && !casita.contains("  - brand-voice\n"), "{casita}");
    assert!(!m.files.values().any(|text| text.contains("locked:")));
    let plan = migrate::plan_text(&m, "sops", &|p| p.to_string());
    assert!(!plan.contains("warning"), "{plan}");
    assert!(plan.contains("  bases/brand-voice.md → instructions/brand-voice.md: removed agents, exclude, locked\n"));
    assert!(plan.contains(
        "  agents/sakura-sushi.yaml: instructions → context; removed inherits, exclude; blocks: restaurant-host, brand-voice, allergen-check, reservations, closing\n"
    ));
    // Even a lock every agent kept.
    let m = migrated("restaurants");
    assert_eq!(m.files["instructions/brand-voice.md"], read(&sops().join("instructions/brand-voice.md")));
}

#[test]
fn migrate_removes_locked_from_a_current_folder() {
    let mut files = read_files(&sops()).unwrap();
    let voice = files["instructions/brand-voice.md"].clone();
    files.insert(
        "instructions/brand-voice.md".into(),
        format!("---\n# Every agent needs this.\nlocked: true\nid: brand-voice\n---\n{voice}"),
    );
    let sop = files["procedures/reservations.yaml"]
        .replace("name: Reservations\n", "name: Reservations\nlocked: yes # all of them\n");
    files.insert("procedures/reservations.yaml".into(), sop);
    assert!(!migrate::is_old(&files) && migrate::needs_migrating(&files));
    let m = migrate::migrate(&files).unwrap();
    assert!(m.removed_fields_only);
    assert_eq!(m.files["instructions/brand-voice.md"], format!("---\nid: brand-voice\n---\n{voice}"));
    assert_eq!(
        m.files,
        read_files(&sops())
            .unwrap()
            .into_iter()
            .map(|(p, t)| {
                let t = if p == "instructions/brand-voice.md" { m.files[&p].clone() } else { t };
                (p, t)
            })
            .collect()
    );
    assert_eq!(migrate::verify(&m.files, &m.expected).unwrap(), 3);
    assert!(!migrate::needs_migrating(&m.files));
    let plan = migrate::plan_text(&m, "sops", &|p| p.to_string());
    assert_eq!(
        plan,
        "Removing fields the format no longer has from sops (2 file(s) change):
  instructions/brand-voice.md: removed locked
  procedures/reservations.yaml: removed locked
Comments that go with removed fields:
  instructions/brand-voice.md
    line 2: # Every agent needs this.
  procedures/reservations.yaml
    line 2: # all of them
Checked: all 3 agent prompt(s) stay the same.
"
    );
    // A ref with locks builds as if migrated (for `plan --against`).
    assert_eq!(migrate::convert(files).unwrap(), m.files);
}

#[test]
fn migrate_refuses_a_partly_migrated_folder() {
    let mut files = read_files(&old_fixture("restaurants").join("sops")).unwrap();
    files.insert("instructions/x.md".into(), "Hi.".into());
    let err = migrate::migrate(&files).err().unwrap();
    assert_eq!(codes(&err.0), ["migrate_failed"]);
    let mut files = read_files(&old_fixture("restaurants").join("sops")).unwrap();
    let agent = files["agents/tonys-pizza.yaml"].clone() + "blocks: [closing]\n";
    files.insert("agents/tonys-pizza.yaml".into(), agent);
    assert_eq!(codes(&migrate::migrate(&files).err().unwrap().0), ["migrate_failed"]);
}

#[test]
fn migrate_convert_leaves_a_current_folder_alone() {
    let files = read_files(&sops()).unwrap();
    assert_eq!(migrate::convert(files.clone()).unwrap(), files);
    let old = read_files(&old_fixture("restaurants").join("sops")).unwrap();
    assert_eq!(migrate::convert(old).unwrap(), files);
}

// --- verify: tools -------------------------------------------------------------------------------

mod verify_tools {
    use crate::verify::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn names(live: &LiveTools) -> Vec<&str> {
        live.names.iter().map(String::as_str).collect()
    }

    #[test]
    fn suggests_near_misses_only() {
        let live: BTreeSet<String> = ["lookup_allergens", "LookupMenu", "end_call"].map(String::from).into();
        assert_eq!(suggest("lookup_alergens", &live).as_deref(), Some("lookup_allergens"));
        assert_eq!(suggest("lookup-menu", &live).as_deref(), Some("LookupMenu"));
        assert_eq!(suggest("place_order", &live), None);
    }

    #[test]
    fn elevenlabs_tools_skip_ids_already_inline() {
        let agent = json!({"conversation_config": {"agent": {"prompt": {
            "tools": [{"type": "webhook", "name": "a", "id": "t_a"}, {"type": "system", "name": "end_call"}],
            "tool_ids": ["t_a", "t_b", ""],
            "built_in_tools": {"transfer_to_number": {"type": "system"}, "skip_turn": null},
        }}}});
        let live = elevenlabs_tools(&agent);
        assert_eq!(names(&live), ["a", "end_call", "transfer_to_number"]);
        assert_eq!(live.ids, BTreeSet::from(["t_b".to_string()]));
        assert_eq!(live.opaque, None);
        let mcp = json!({"conversation_config": {"agent": {"prompt": {"mcp_server_ids": ["m1"]}}}});
        assert!(elevenlabs_tools(&mcp).opaque.is_some());
    }

    #[test]
    fn vapi_tools_use_the_names_the_model_sees() {
        let assistant = json!({"endCallFunctionEnabled": true, "model": {
            "tools": [{"type": "function", "function": {"name": "f"}}, {"type": "transferCall"},
                      {"type": "transferCall", "function": {"name": "transfer_to_billing"}}],
            "toolIds": ["t1"],
        }});
        let live = vapi_tools(&assistant);
        assert_eq!(names(&live), ["endCall", "f", "transferCall", "transfer_to_billing"]);
        assert_eq!(live.ids, BTreeSet::from(["t1".to_string()]));
    }

    #[test]
    fn retell_flow_tools_note_shared_tools() {
        let flow = json!({"tools": [{"name": "a", "tool_id": "x"}],
                          "nodes": [{"type": "function", "tool_id": "s1", "tool_type": "shared"}]});
        let live = retell_flow_tools(&flow);
        assert_eq!(names(&live), ["a"]);
        assert!(live.opaque.is_some());
    }

    #[test]
    fn tools_arent_checked_without_a_platform_list() {
        let refs = vec![ToolRef { name: "a".into(), required: true }];
        let tools = compare_tools(refs, ToolList::NotChecked("custom LLM".into()));
        assert!(!tools.checked && tools.missing.is_empty() && tools.error.is_none());
    }
}
