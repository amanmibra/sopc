//! Tests of the sopc binary: commands, flags, exit codes and files it writes.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn fixture() -> PathBuf {
    repo_root().join("tests/fixtures/restaurants")
}
fn example() -> PathBuf {
    repo_root().join("examples/livekit-restaurant/sops")
}
fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

fn sopc_in(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sopc"));
    cmd.args(args).current_dir(dir).env_remove("GITHUB_OUTPUT").env_remove("GITHUB_STEP_SUMMARY");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    Output {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// Runs sopc and checks the exit code.
fn run(want: i32, args: &[&str]) -> Output {
    let out = sopc_in(&repo_root(), args, &[]);
    assert_eq!(out.code, want, "sopc {args:?}\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
    out
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

fn edit(path: &Path, old: &str, new: &str) {
    let text = read(path);
    assert!(text.contains(old), "{old:?} not in {}", path.display());
    std::fs::write(path, text.replace(old, new)).unwrap();
}

/// A temp folder holding a copy of the fixture's sops/.
fn repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixture().join("sops"), &dir.path().join("sops"));
    let root = dir.path().join("sops").to_string_lossy().into_owned();
    (dir, root)
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A git repo with the fixture committed at sops/ on main.
fn git_repo() -> (tempfile::TempDir, String) {
    let (dir, root) = repo();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    (dir, root)
}

#[test]
fn compile_check_and_plan() {
    let (dir, root) = git_repo();
    run(0, &["--dir", &root]);
    run(0, &["--dir", &root, "--check"]);
    edit(&Path::new(&root).join("instructions/closing.md"), "repeat the order total", "repeat the order and total");
    let out = run(1, &["--dir", &root, "--check"]);
    assert!(out.stderr.contains("is out of date; run `sopc`"), "{}", out.stderr);
    // No --against: compares with main, the only branch.
    let out = sopc_in(dir.path(), &["plan", "--summary"], &[]);
    assert!(out.stdout.contains("instruction `closing` edited → 3 agents"), "{}", out.stdout);
}

#[test]
fn compile_prints_a_tidy_output_path() {
    let (dir, _) = repo();
    let out = sopc_in(dir.path(), &["--dir", "./sops/"], &[]);
    assert_eq!(out.stdout, "wrote 6 files to sops/build\n");
}

#[test]
fn bare_sopc_compiles_the_default_folder() {
    let (dir, _) = repo();
    let out = sopc_in(dir.path(), &[], &[]);
    assert_eq!((out.code, out.stdout.as_str()), (0, "wrote 6 files to sops/build\n"), "{}", out.stderr);
    let out = sopc_in(dir.path(), &["--check"], &[]);
    assert_eq!((out.code, out.stdout.as_str()), (0, "sops/build is up to date\n"), "{}", out.stderr);
    let out = sopc_in(dir.path(), &["-o", "dist"], &[]);
    assert_eq!(out.stdout, "wrote 6 files to dist\n");
    let out = sopc_in(dir.path(), &["--out", "dist", "--check"], &[]);
    assert_eq!(out.stdout, "dist is up to date\n");
    for cmd in [&["validate"][..], &["lint"], &["fmt", "--check"], &["agents"]] {
        assert_eq!(sopc_in(dir.path(), cmd, &[]).code, 0, "{cmd:?}");
    }
    // An explicit folder wins over the default one.
    let out = sopc_in(dir.path(), &["--dir", "sops", "-o", "elsewhere"], &[]);
    assert_eq!(out.stdout, "wrote 6 files to elsewhere\n");
    let sops = dir.path().join("sops");
    assert_eq!(sopc_in(&sops, &["--dir", "."], &[]).stdout, "wrote 6 files to build\n");
}

#[test]
fn compile_keeps_files_it_did_not_build() {
    let (dir, _) = repo();
    // An unrelated file in the output folder survives a rebuild.
    run_ok(dir.path(), &[]);
    std::fs::write(dir.path().join("sops/build/README.md"), "mine").unwrap();
    run_ok(dir.path(), &[]);
    assert_eq!(read(&dir.path().join("sops/build/README.md")), "mine");
    // A non-empty folder without lock.json is refused, unless --force.
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs/notes.prompt.md"), "mine").unwrap();
    let out = sopc_in(dir.path(), &["-o", "docs"], &[]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("docs has files but no lock.json"), "{}", out.stderr);
    assert!(!dir.path().join("docs/lock.json").exists());
    assert_eq!(run_ok(dir.path(), &["-o", "docs", "--force"]).stdout, "wrote 6 files to docs\n");
    assert_eq!(read(&dir.path().join("docs/notes.prompt.md")), "mine");
    // --force is a compile flag.
    assert_eq!(sopc_in(dir.path(), &["--force", "validate"], &[]).code, 2);
}

fn run_ok(dir: &Path, args: &[&str]) -> Output {
    let out = sopc_in(dir, args, &[]);
    assert_eq!(out.code, 0, "sopc {args:?}\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
    out
}

#[test]
fn default_folder_is_sops_then_the_current_one() {
    let (dir, _) = repo();
    // sops/sopc.yaml wins even when ./sopc.yaml exists too.
    copy_dir(&fixture().join("sops"), &dir.path().join("other"));
    std::fs::copy(dir.path().join("sops/sopc.yaml"), dir.path().join("sopc.yaml")).unwrap();
    assert_eq!(sopc_in(dir.path(), &[], &[]).stdout, "wrote 6 files to sops/build\n");
    // Inside the folder itself, ./ is used.
    let other = dir.path().join("other");
    assert_eq!(sopc_in(&other, &[], &[]).stdout, "wrote 6 files to build\n");
    assert_eq!(sopc_in(&other, &["validate"], &[]).code, 0);
    // Neither: an error that says what to pass.
    let empty = tempfile::tempdir().unwrap();
    for args in [&[][..], &["validate"], &["plan"], &["convert", "--to", "md"]] {
        let out = sopc_in(empty.path(), args, &[]);
        assert_eq!(out.code, 1, "{args:?}");
        assert!(
            out.stderr.contains(
                "error [missing_config] no sopc.yaml in ./sops or ./ (pass the folder, e.g. `sopc --dir path/to/sops`)"
            ),
            "{args:?}: {}",
            out.stderr
        );
    }
    // A default folder with the old config name still explains the rename.
    let legacy = tempfile::tempdir().unwrap();
    copy_dir(&fixture().join("sops"), &legacy.path().join("sops"));
    std::fs::rename(legacy.path().join("sops/sopc.yaml"), legacy.path().join("sops/opensop.yaml")).unwrap();
    let out = sopc_in(legacy.path(), &["validate"], &[]);
    assert!(out.stderr.contains("rename opensop.yaml to sopc.yaml"), "{}", out.stderr);
}

#[test]
fn the_folder_is_a_flag_on_every_command() {
    let (dir, _) = repo();
    // --dir / -C goes before or after the command.
    for args in [&["--dir", "sops", "validate"][..], &["validate", "--dir", "sops"], &["-C", "sops", "lint"]] {
        assert_eq!(sopc_in(dir.path(), args, &[]).code, 0, "{args:?}");
    }
    // Typos are unknown commands with a suggestion, never folders.
    let out = sopc_in(dir.path(), &["valdate"], &[]);
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("a similar subcommand exists: 'validate'"), "{}", out.stderr);
    let out = sopc_in(dir.path(), &["--dir", "nope"], &[]);
    assert!(out.stderr.contains("nope is not a folder"), "{}", out.stderr);
    // Compile-only flags can't be combined with a command.
    let out = sopc_in(dir.path(), &["--check", "validate"], &[]);
    assert_eq!(out.code, 2);
    assert!(out.stderr.contains("apply to compiling"), "{}", out.stderr);
}

#[test]
fn plan_with_nothing_to_compare_against() {
    let (_dir, root) = repo();
    let out = run(1, &["plan", "--dir", &root]);
    assert!(out.stderr.contains("nothing to compare against; pass --against <ref>"), "{}", out.stderr);
    // affected selects every agent, and warns unless that was asked for.
    let out = run(0, &["affected", "--dir", &root]);
    assert_eq!(out.stdout.lines().count(), 3, "every agent: {}", out.stdout);
    let warning = "warning: no default branch found (origin/HEAD, main, master); selecting every agent. \
                   Pass --against REF to compare.\n";
    assert_eq!(out.stderr, warning);
    for flag in [&["--all-if-none"][..], &["--agents", "tonys-pizza"]] {
        let out = run(0, &[&["affected", "--dir", &root][..], flag].concat());
        assert_eq!(out.stderr, "", "{flag:?}");
    }
}

#[test]
fn plan_and_affected_say_what_they_compared_with() {
    let (dir, _) = git_repo();
    let sha = String::from_utf8(
        Command::new("git").args(["rev-parse", "--short", "main"]).current_dir(dir.path()).output().unwrap().stdout,
    )
    .unwrap();
    let line = format!("Comparing with main ({})\n", sha.trim());
    for args in [&["plan"][..], &["plan", "--json"], &["affected"], &["affected", "--against", "main"]] {
        let out = sopc_in(dir.path(), args, &[]);
        assert_eq!((out.code, out.stderr.as_str()), (0, line.as_str()), "{args:?}");
        assert!(!out.stdout.contains("Comparing"), "{args:?}: stdout stays clean");
    }
    let summary = dir.path().join("summary");
    sopc_in(dir.path(), &["affected", "--ci"], &[("GITHUB_STEP_SUMMARY", &summary)]);
    assert!(read(&summary).contains(&format!("Compared with `main ({})`.", sha.trim())), "{}", read(&summary));
}

#[test]
fn default_ref_prefers_origin_head_and_explicit_against_wins() {
    let (dir, root) = git_repo();
    git(dir.path(), &["branch", "trunk"]);
    edit(&Path::new(&root).join("instructions/closing.md"), "repeat the order total", "repeat the order and total");
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", "edit"]);
    // main is HEAD now: no change against it.
    let out = sopc_in(dir.path(), &["plan", "--summary"], &[]);
    assert!(out.stdout.contains("No agent prompts change."), "{}", out.stdout);
    let out = sopc_in(dir.path(), &["plan", "--summary", "--against", "trunk"], &[]);
    assert!(out.stdout.contains("instruction `closing` edited → 3 agents"), "{}", out.stdout);
    // origin/HEAD, when set, comes before main.
    git(dir.path(), &["update-ref", "refs/remotes/origin/trunk", "trunk"]);
    git(dir.path(), &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
    let out = sopc_in(dir.path(), &["affected"], &[]);
    assert_eq!(out.stdout, "luigis-trattoria\nsakura-sushi\ntonys-pizza\n", "{}", out.stderr);
    let out = sopc_in(dir.path(), &["affected", "--against", "main"], &[]);
    assert_eq!(out.stdout, "");
}

#[test]
fn plan_against_a_git_ref() {
    let (_dir, root) = git_repo();
    edit(
        &Path::new(&root).join("procedures/allergen-check.yaml"),
        "Name the specific allergen",
        "Repeat the specific allergen",
    );
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert!(out.stdout.contains("SOP `allergen-check` edited → 3 agents"), "{}", out.stdout);
    let out = run(1, &["plan", "--dir", &root, "--against", "nope"]);
    let want = "sopc: error: unknown git ref `nope`; check the name, or fetch it (`git fetch origin nope`)\n";
    assert_eq!(out.stderr, want);
    let out = run(1, &["affected", "--dir", &root, "--against", "origin/nope"]);
    assert!(out
        .stderr
        .contains("unknown git ref `origin/nope`; check the name, or fetch it (`git fetch origin nope`)"));
    // Outside a git repo.
    let (_dir, root) = repo();
    for cmd in ["plan", "affected"] {
        let out = run(1, &[cmd, "--dir", &root, "--against", "main"]);
        assert_eq!(out.stderr, "sopc: error: not a git repository; --against needs git\n", "{cmd}");
    }
}

#[test]
fn plan_reads_every_file_at_a_ref() {
    // CRLF text, non-UTF-8 and non-source files, and files outside the sopc folder, all at once.
    let (dir, root) = repo();
    let host = Path::new(&root).join("instructions/restaurant-host.md");
    std::fs::write(&host, read(&host).replace('\n', "\r\n")).unwrap();
    std::fs::write(Path::new(&root).join("notes ü.txt"), b"\xff\xfe binary\n").unwrap();
    std::fs::write(dir.path().join("other.yaml"), "x: 1\n").unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["-c", "core.autocrlf=false", "add", "."]);
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert!(out.stdout.contains("No agent prompts change."), "{}", out.stdout);
    edit(&host, "phone host", "host");
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert!(out.stdout.contains("instruction `restaurant-host` edited"), "{}", out.stdout);
}

#[test]
fn plan_against_a_ref_from_before_the_rename() {
    let (dir, root) = repo();
    std::fs::rename(Path::new(&root).join("sopc.yaml"), Path::new(&root).join("opensop.yaml")).unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    let out = run(1, &["validate", "--dir", &root]);
    assert!(out.stderr.contains("rename opensop.yaml to sopc.yaml"), "{}", out.stderr);
    git(dir.path(), &["mv", "sops/opensop.yaml", "sops/sopc.yaml"]);
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert!(out.stdout.contains("No agent prompts change."), "{}", out.stdout);
}

#[test]
fn validate_reports_errors() {
    let (dir, root) = repo();
    edit(&Path::new(&root).join("agents/tonys-pizza.yaml"), "  - pizza-context\n", "  - nope\n  - pizza-context\n");
    let out = run(1, &["validate", "--dir", &root]);
    assert!(out.stderr.contains("agents/tonys-pizza.yaml: error [unknown_block]"), "{}", out.stderr);
    assert_eq!(out.stdout, "1 error(s), 0 warning(s)\n");
    let out = run(1, &["validate", "--dir", &root, "--json"]);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["issues"][0]["code"], "unknown_block");
    assert_eq!(report["issues"][0]["path"], "agents/tonys-pizza.yaml");
    // Files that don't parse are reported the same way.
    std::fs::write(Path::new(&root).join("agents/broken.yaml"), "livekit: [").unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&run(1, &["validate", "--dir", &root, "--json"]).stdout).unwrap();
    assert!(report["issues"].as_array().unwrap().iter().any(|i| i["path"] == "agents/broken.yaml"), "{report}");
    std::fs::remove_file(Path::new(&root).join("agents/broken.yaml")).unwrap();
    // Paths are printed from the current folder, for validate and the commands that load files.
    for args in [&["validate"][..], &["agents"], &[]] {
        let out = sopc_in(dir.path(), args, &[]);
        assert!(
            out.stderr.starts_with("sops/agents/tonys-pizza.yaml: error [unknown_block]"),
            "{args:?}: {}",
            out.stderr
        );
    }
}

#[test]
fn example_is_valid_and_its_build_is_current() {
    let ex = example().to_string_lossy().into_owned();
    let out = run(0, &["validate", "--dir", &ex]);
    assert_eq!((out.stdout.as_str(), out.stderr.as_str()), ("0 error(s), 0 warning(s)\n", ""));
    run(0, &["--dir", &ex, "--check"]);
    run(0, &["lint", "--dir", &ex]);
    run(0, &["fmt", "--dir", &ex, "--check"]);
}

fn built(root: &str, out: &Path) -> std::collections::BTreeMap<String, String> {
    run(0, &["--dir", root, "--out", &out.to_string_lossy()]);
    let files = std::fs::read_dir(out).unwrap().map(|e| e.unwrap().path());
    files
        .filter(|p| !p.ends_with("lock.json"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), read(&p)))
        .collect()
}

fn sop_files(root: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(Path::new(root).join("procedures"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn convert_round_trip_keeps_every_prompt_and_tool_json() {
    for src in [fixture().join("sops"), example()] {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&src, &dir.path().join("s"));
        let root = dir.path().join("s").to_string_lossy().into_owned();
        let before = built(&root, &dir.path().join("b0"));
        let originals: Vec<(String, String)> = sop_files(&root)
            .into_iter()
            .map(|n| (n.clone(), read(&Path::new(&root).join("procedures").join(&n))))
            .collect();
        let out = run(0, &["convert", "--dir", &root, "--to", "md"]);
        assert!(out.stdout.contains(".yaml -> "), "{}", out.stdout);
        assert!(sop_files(&root).iter().all(|n| n.ends_with(".md")));
        assert_eq!(built(&root, &dir.path().join("b1")), before);
        run(0, &["fmt", "--dir", &root, "--check"]);
        if src == example() {
            // The example's front matter has trailing comments that YAML output can't keep:
            // converting back refuses until approved, and lists them.
            let out = run(1, &["convert", "--dir", &root, "--to", "yaml"]);
            assert!(out.stderr.contains("not converted: it would remove"), "{}", out.stderr);
            run(0, &["convert", "--dir", &root, "--to", "yaml", "--yes"]);
        } else {
            run(0, &["convert", "--dir", &root, "--to", "yaml"]);
        }
        assert_eq!(built(&root, &dir.path().join("b2")), before);
        if src == fixture().join("sops") {
            let now: Vec<(String, String)> = sop_files(&root)
                .into_iter()
                .map(|n| (n.clone(), read(&Path::new(&root).join("procedures").join(&n))))
                .collect();
            assert_eq!(now, originals, "the fixture's YAML comes back byte for byte");
        }
    }
}

#[test]
fn convert_some_sops_and_unknown_ids() {
    let (dir, root) = repo();
    // Paths are printed from the current folder.
    let out = sopc_in(dir.path(), &["convert", "--to", "md", "reservations"], &[]);
    assert_eq!(out.stdout, "converted sops/procedures/reservations.yaml -> sops/procedures/reservations.md\n");
    assert!(sop_files(&root).contains(&"allergen-check.yaml".to_string()));
    let names = std::fs::read_dir(Path::new(&root).join("procedures")).unwrap();
    assert!(names.map(|e| e.unwrap().file_name()).all(|n| !n.to_string_lossy().starts_with('.')), "no temp files");
    let out = run(1, &["convert", "--dir", &root, "--to", "md", "nope"]);
    assert!(out.stderr.contains("error [unknown_sop] 'nope' is not an SOP"), "{}", out.stderr);
    let out = run(0, &["convert", "--dir", &root, "--to", "md", "reservations"]);
    assert_eq!(out.stdout, "nothing to convert\n");
}

#[test]
fn fmt_check_exit_codes() {
    let (dir, root) = repo();
    let out = run(0, &["fmt", "--dir", &root, "--check"]);
    assert!(out.stdout.contains("4 YAML SOP file(s) not checked; add --yaml"), "{}", out.stdout);
    let out = run(0, &["fmt", "--dir", &root, "--check", "--yaml"]);
    assert_eq!(out.stdout, "0 SOP file(s) would be reformatted, 4 already formatted\n");
    let file = Path::new(&root).join("procedures/reservations.yaml");
    edit(&file, "name: Reservations\n", "\nname:   Reservations\ndelivery: prompt\n");
    let messy = read(&file);
    run(0, &["fmt", "--dir", &root]); // YAML isn't touched without --yaml
    assert_eq!(read(&file), messy);
    let out = sopc_in(dir.path(), &["fmt", "--check", "--yaml"], &[]);
    assert_eq!((out.code, out.stdout.as_str()), (1, "would reformat sops/procedures/reservations.yaml\n"));
    assert_eq!(read(&file), messy, "--check writes nothing");
    let out = sopc_in(&Path::new(&root).join("procedures"), &["fmt", "-C", "..", "--yaml"], &[]);
    assert!(out.stdout.starts_with("formatted ../procedures/reservations.yaml\n"), "{}", out.stdout);
    assert!(read(&file).starts_with("name: Reservations\ndescription:"));
    run(0, &["fmt", "--dir", &root, "--check", "--yaml"]);
    std::fs::write(Path::new(&root).join("procedures/broken.md"), "# Broken\n## Notes\n").unwrap();
    let out = sopc_in(dir.path(), &["fmt"], &[]);
    assert!(out.stderr.starts_with("sops/procedures/broken.md: error [md_unknown_section] line 2:"), "{}", out.stderr);
}

#[test]
fn guide_prints_the_format_reference() {
    let out = run(0, &["guide"]);
    assert_eq!(out.stdout, read(&repo_root().join("FORMAT.md")));
}

#[test]
fn printed_docs_link_absolutely() {
    // `sopc guide` and installed skills are read inside other repos, where relative links are dead.
    for file in ["FORMAT.md", "skills/sopc-import/SKILL.md"] {
        let text = read(&repo_root().join(file));
        for (i, _) in text.match_indices("](") {
            let target = &text[i + 2..];
            assert!(
                target.starts_with("https://") || target.starts_with('#'),
                "{file}: relative link {}",
                target.lines().next().unwrap_or_default()
            );
        }
    }
}

#[test]
fn format_reference_lists_every_validation_code() {
    let guide = read(&repo_root().join("FORMAT.md"));
    let source = ["workspace.rs", "sopfile.rs", "migrate.rs", "render.rs"]
        .map(|f| read(&repo_root().join("src").join(f)))
        .join("\n");
    let re = regex::Regex::new(
        r#"(?:Issue::(?:error|warning)\(|r\.(?:error|warning)\([^"]*?|fn failed[^"]*?)\s*"([a-z_]+)""#,
    )
    .unwrap();
    let codes: std::collections::BTreeSet<&str> =
        re.captures_iter(&source).map(|c| c.get(1).unwrap().as_str()).collect();
    assert_eq!(codes.len(), 35, "{codes:?}");
    for code in codes {
        assert!(guide.contains(&format!("`{code}`")), "FORMAT.md doesn't document `{code}`");
    }
}

#[test]
fn plan_json_and_agents_json() {
    let (_dir, root) = git_repo();
    run(0, &["--dir", &root]);
    edit(
        &Path::new(&root).join("procedures/reservations.yaml"),
        "Never double-book a table",
        "Never double-book or overbook a table",
    );
    let plan: serde_json::Value =
        serde_json::from_str(&run(0, &["plan", "--dir", &root, "--against", "main", "--json"]).stdout).unwrap();
    let got: Vec<String> = plan["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| format!("{} {} {}", c["agent"], c["platform_ref"], c["status"]))
        .collect();
    assert_eq!(
        got,
        [
            r#""luigis-trattoria" "livekit:luigis-trattoria" "changed""#,
            r#""sakura-sushi" "livekit:sakura-sushi" "changed""#
        ]
    );
    let agents: serde_json::Value =
        serde_json::from_str(&run(0, &["agents", "--dir", &root, "--json"]).stdout).unwrap();
    let sakura = agents.as_array().unwrap().iter().find(|a| a["id"] == "sakura-sushi").unwrap();
    assert_eq!(sakura["platform_id"], "sakura-sushi");
    assert_eq!(sakura["sops"], serde_json::json!(["allergen-check", "reservations"]));
    assert_eq!(sakura["instructions"], serde_json::json!(["restaurant-host", "brand-voice", "closing"]));
    assert_eq!(
        sakura["blocks"],
        serde_json::json!(["restaurant-host", "brand-voice", "allergen-check", "reservations", "closing"])
    );
    let tonys = agents.as_array().unwrap().iter().find(|a| a["id"] == "tonys-pizza").unwrap();
    assert!(tonys["tools"].as_array().unwrap().contains(&serde_json::json!("transfer_to_staff")));
}

#[test]
fn affected_against_a_git_ref_writes_github_outputs() {
    let (dir, root) = git_repo();
    edit(
        &Path::new(&root).join("procedures/allergen-check.yaml"),
        "Name the specific allergen",
        "Repeat the specific allergen",
    );
    let (output, summary) = (dir.path().join("out"), dir.path().join("summary"));
    let out = sopc_in(
        dir.path(),
        &["affected", "--dir", &root, "--against", "main", "--all-if-none", "--ci"],
        &[("GITHUB_OUTPUT", &output), ("GITHUB_STEP_SUMMARY", &summary)],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout, "luigis-trattoria\nsakura-sushi\ntonys-pizza\n");
    let outputs: std::collections::BTreeMap<String, String> = read(&output)
        .lines()
        .map(|l| l.split_once('=').unwrap())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(outputs["ids"], "luigis-trattoria sakura-sushi tonys-pizza");
    assert_eq!((outputs["count"].as_str(), outputs["all"].as_str()), ("3", "false"));
    let matrix: serde_json::Value = serde_json::from_str(&outputs["matrix"]).unwrap();
    assert_eq!(matrix[0]["changed_sops"], serde_json::json!(["allergen-check"]));
    assert!(read(&summary).contains("`tonys-pizza` (livekit `tonys-pizza`): sop:allergen-check"));
}

#[test]
fn affected_without_github_output_shows_what_it_would_write() {
    let (_dir, root) = repo();
    let out = run(0, &["affected", "--dir", &root, "--agents", "tonys-pizza", "--ci"]);
    assert!(
        out.stderr.contains("# GITHUB_OUTPUT not set; these would be written:\n#   ids=tonys-pizza\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn affected_json_format_and_unknown_agents() {
    let (_dir, root) = repo();
    let data: serde_json::Value = serde_json::from_str(
        &run(0, &["affected", "--dir", &root, "--agents", "tonys-pizza", "--format", "json"]).stdout,
    )
    .unwrap();
    assert_eq!(data["count"], 1);
    assert_eq!(data["agents"][0]["reason"], "requested");
    assert_eq!(data["agents"][0]["sops"], serde_json::json!(["allergen-check", "delivery-handling", "large-orders"]));
    let out = run(1, &["affected", "--dir", &root, "--agents", "nope,tonys-pizza"]);
    assert!(out.stderr.starts_with("error [unknown_agent] unknown agent(s): nope. Known: "), "{}", out.stderr);
}

#[test]
fn compare_exit_code() {
    let sops = fixture().join("sops");
    let originals = fixture().join("originals");
    let out = run(1, &["compare", "--dir", sops.to_str().unwrap(), "--originals", originals.to_str().unwrap()]);
    assert!(out.stdout.contains("('twice' → 'once')"), "{}", out.stdout);
    let out = run(
        1,
        &["compare", "--dir", sops.to_str().unwrap(), "--originals", repo_root().join("spec").to_str().unwrap()],
    );
    assert!(out.stderr.contains("error [no_prompts]"), "{}", out.stderr);
}

/// A copy of the fixture with one more sentence in tonys-pizza's `context`, which the shared
/// `brand-voice` instruction contradicts or not ("Never upsell more than once per call.").
fn lint_with_context(sentence: &str) -> (Output, Output) {
    let (_dir, root) = repo();
    edit(
        &Path::new(&root).join("agents/tonys-pizza.yaml"),
        "Pickup only after 10pm.",
        &format!("Pickup only after 10pm. {sentence}"),
    );
    (
        sopc_in(&repo_root(), &["lint", "--dir", &root, "--json"], &[]),
        sopc_in(&repo_root(), &["lint", "--dir", &root, "--strict"], &[]),
    )
}

#[test]
fn lint_strict_fails_on_a_context_that_contradicts_a_shared_rule() {
    for (sentence, code) in [
        ("Never upsell more than twice per call.", "numeric_conflict"),
        ("Always upsell more than once per call.", "negation_conflict"),
    ] {
        let (json, strict) = lint_with_context(sentence);
        assert_eq!(json.code, 0, "without --strict lint exits 0");
        let findings: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
        assert_eq!(findings.as_array().unwrap().len(), 1, "{sentence}: {findings}");
        assert_eq!(findings[0]["code"], code, "{sentence}");
        assert_eq!(findings[0]["agents"], serde_json::json!(["tonys-pizza"]));
        assert_eq!(
            findings[0]["sources"],
            serde_json::json!([
                {"block": "agent `tonys-pizza`", "text": sentence},
                {"block": "instruction `brand-voice`", "text": "Never upsell more than once per call."},
            ])
        );
        assert_eq!(strict.code, 1, "{sentence}: --strict fails\n{}", strict.stdout);
        assert!(strict.stdout.contains(&format!("[{code}]")), "{}", strict.stdout);
        assert_eq!(strict.stderr, "1 finding(s); failing because of --strict\n");
    }
}

#[test]
fn lint_does_not_catch_a_paraphrased_contradiction() {
    // Known limitation: lint compares wording, not meaning. "Up to 2 times" contradicts "never more
    // than once", but the sentences share too few words. If a semantic check starts catching this,
    // update the test (and CLI.md) on purpose.
    let (json, strict) = lint_with_context("Upsell up to 2 times per call.");
    assert_eq!(json.stdout, "[]\n");
    assert_eq!(strict.code, 0, "{}", strict.stdout);
}

#[test]
fn overlap_and_lint_run() {
    let out = run(0, &["overlap", fixture().join("originals").to_str().unwrap()]);
    assert!(out.stdout.starts_with("3 prompts: luigis-trattoria, sakura-sushi, tonys-pizza\n"));
    assert!(out.stdout.contains("Near-copies"));
    let out = run(0, &["lint", "--dir", fixture().join("sops").to_str().unwrap(), "--json"]);
    assert_eq!(out.stdout, "[]\n");
    run(0, &["lint", "--dir", fixture().join("sops").to_str().unwrap(), "--strict"]);
}

#[test]
fn skills_install_covers_claude_code_codex_and_opencode() {
    let dir = tempfile::tempdir().unwrap();
    let out = sopc_in(dir.path(), &["skills", "install"], &[]);
    assert_eq!(out.code, 0);
    let skill = read(&repo_root().join("skills/sopc-import/SKILL.md"));
    for folder in [".claude/skills", ".agents/skills"] {
        assert_eq!(read(&dir.path().join(folder).join("sopc-import/SKILL.md")), skill);
    }
    for cmd in ["sopc overlap", "sopc compare", "sopc lint", "sopc guide"] {
        assert!(skill.contains(cmd), "skill doesn't mention {cmd}");
    }
    for s in ["/sopc-import", "$sopc-import", "OpenCode"] {
        assert!(out.stdout.contains(s), "output lacks {s}");
    }
}

#[test]
fn skills_install_for_one_agent_or_folder() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(sopc_in(dir.path(), &["skills", "install", "--agent", "codex"], &[]).code, 0);
    assert!(dir.path().join(".agents/skills/sopc-import/SKILL.md").exists());
    assert!(!dir.path().join(".claude").exists());
    let out = sopc_in(dir.path(), &["skills", "install", "--into", "custom"], &[]);
    assert_eq!(out.stdout, "installed custom/sopc-import/SKILL.md\n\n");
}

#[test]
fn argument_errors_exit_2() {
    for args in [
        &["--foo"][..],
        &["sops"],
        &["check"],
        &["validate", "--jso"],
        &["affected", "--format", "xx"],
        &["compare"],
        &["--out"],
        &["skills", "foo"],
    ] {
        let out = run(2, args);
        assert!(out.stderr.contains("error:"), "{args:?}: {}", out.stderr);
    }
    let out = run(0, &["--help"]);
    assert!(
        out.stdout.contains("Usage: sopc [--dir DIR] [-o DIR] [--check]\n       sopc [--dir DIR] <COMMAND>"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("Run `sopc` to compile"), "{}", out.stdout);
    let out = run(0, &["plan", "-h"]);
    assert!(out.stdout.contains("Usage: sopc plan [OPTIONS]"), "{}", out.stdout);
    assert!(out.stdout.contains("-C, --dir <DIR>"), "{}", out.stdout);
}

#[test]
fn export_prints_every_block_as_json() {
    let (_dir, root) = repo();
    let out: serde_json::Value = serde_json::from_str(&run(0, &["export", "--dir", &root]).stdout).unwrap();
    assert_eq!(out["config"]["groups"], serde_json::json!({}));
    assert_eq!(out["config"]["sops_heading"], "## Procedures");
    assert_eq!(out["config"]["variables"]["staff_transfer"], "the manager on duty");
    let instruction = out["instructions"].as_array().unwrap().iter().find(|b| b["id"] == "brand-voice").unwrap();
    assert_eq!(
        instruction,
        &serde_json::json!({"id": "brand-voice", "file": "instructions/brand-voice.md",
            "text": "Speak warmly and briefly. Ask one question at a time. Never upsell more than once per call."})
    );
    let sop = out["sops"].as_array().unwrap().iter().find(|s| s["id"] == "allergen-check").unwrap();
    assert_eq!(sop["file"], "procedures/allergen-check.yaml");
    assert_eq!(sop["procedureSteps"][0], "Ask if anyone in the order has a food allergy");
    assert_eq!(
        sop["procedureSteps"][2],
        serde_json::json!({"text": "Check each item the customer ordered against {{menu_allergen_link}}", "tool": "lookup_allergens", "required": true})
    );
    let agent = out["agents"].as_array().unwrap().iter().find(|a| a["id"] == "sakura-sushi").unwrap();
    assert_eq!(agent["platform"], "livekit");
    assert_eq!(agent["platform_id"], "sakura-sushi");
    assert_eq!(agent["retell"], serde_json::Value::Null);
    assert_eq!(
        agent["blocks"],
        serde_json::json!(["restaurant-host", "brand-voice", "allergen-check", "reservations", "closing"])
    );
    assert!(agent["context"].as_str().unwrap().starts_with("Sakura is an omakase"));
    let vars: Vec<&String> = agent["variables"].as_object().unwrap().keys().collect();
    assert_eq!(vars, ["restaurant_name", "menu_allergen_link", "staff_transfer"]); // in file order

    // Markdown SOPs read into the same fields.
    run(0, &["convert", "--to", "md", "allergen-check", "--dir", &root]);
    let md: serde_json::Value = serde_json::from_str(&run(0, &["export", "--dir", &root]).stdout).unwrap();
    let md_sop = md["sops"].as_array().unwrap().iter().find(|s| s["id"] == "allergen-check").unwrap();
    assert_eq!(md_sop["file"], "procedures/allergen-check.md");
    assert_eq!(md_sop["procedureSteps"], sop["procedureSteps"]);

    // Files that don't parse: the issues, as validate --json prints them, and exit 1.
    std::fs::write(Path::new(&root).join("agents/broken.yaml"), "livekit: [").unwrap();
    let out: serde_json::Value = serde_json::from_str(&run(1, &["export", "--dir", &root]).stdout).unwrap();
    assert_eq!(out["valid"], false);
    assert_eq!(out["issues"][0]["path"], "agents/broken.yaml");
    // Only parsing matters: a reference to a missing block is validate's business.
    std::fs::remove_file(Path::new(&root).join("agents/broken.yaml")).unwrap();
    edit(&Path::new(&root).join("agents/tonys-pizza.yaml"), "  - pizza-context\n", "  - nope\n");
    run(0, &["export", "--dir", &root]);
    // Groups, in order.
    edit(&Path::new(&root).join("sopc.yaml"), "version: 1\n", "version: 1\ngroups:\n  b: [closing]\n  a: [b]\n");
    let out: serde_json::Value = serde_json::from_str(&run(0, &["export", "--dir", &root]).stdout).unwrap();
    assert_eq!(out["config"]["groups"].to_string(), r#"{"b":["closing"],"a":["b"]}"#);
}

#[test]
fn help_has_examples_and_docs_links() {
    let docs = "https://github.com/amanmibra/sopc/blob/main/CLI.md";
    let out = run(0, &["--help"]);
    assert!(out.stdout.contains("Examples:\n  sopc "), "{}", out.stdout);
    assert!(out.stdout.ends_with(&format!("Docs: {docs}\nIssues: https://github.com/amanmibra/sopc/issues\n")));
    assert!(!run(0, &["-h"]).stdout.contains("Examples:"));
    for cmd in [
        "validate", "lint", "fmt", "plan", "affected", "agents", "export", "convert", "overlap", "compare", "verify",
        "migrate", "skills", "guide",
    ] {
        let link = format!("Docs: {docs}#sopc-{cmd}\n");
        let long = run(0, &[cmd, "--help"]).stdout;
        assert!(long.contains(&format!("Examples:\n  sopc {cmd}")) && long.ends_with(&link), "{cmd}: {long}");
        let short = run(0, &[cmd, "-h"]).stdout;
        assert!(!short.contains("Examples:") && short.ends_with(&link), "{cmd}: {short}");
    }
}

#[test]
fn a_closed_pipe_is_a_quiet_exit() {
    // Like `sopc guide | head -1`: the reader is gone before sopc writes.
    for args in [&["guide"][..], &["agents", "--dir", "tests/fixtures/restaurants/sops"]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sopc"))
            .args(args)
            .current_dir(repo_root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!((out.status.code(), stderr.as_ref()), (Some(0), ""), "{args:?}");
    }
}

#[test]
fn rewrites_that_would_remove_comments_need_yes() {
    let (_dir, root) = repo();
    let file = Path::new(&root).join("procedures/reservations.yaml");
    edit(&file, "name: Reservations\n", "name:   Reservations   # the heading\n");
    edit(&file, "procedureSteps:\n", "procedureSteps:\n  # ask first\n");
    let original = read(&file);

    let out = run(1, &["fmt", "--dir", &root, "--yaml"]);
    assert!(
        out.stderr.contains("procedures/reservations.yaml: not formatted: it would remove 2 comment(s)"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("# the heading") && out.stderr.contains("# ask first"), "{}", out.stderr);
    assert_eq!(read(&file), original, "nothing is written without --yes");
    run(1, &["fmt", "--dir", &root, "--yaml", "--check"]);

    let out = run(1, &["convert", "--dir", &root, "--to", "md", "reservations"]);
    assert!(out.stderr.contains("not converted: it would remove 2 comment(s)"), "{}", out.stderr);
    assert_eq!(read(&file), original);

    run(0, &["fmt", "--dir", &root, "--yaml", "-y"]);
    assert!(!read(&file).contains("# ask first"));
    run(0, &["fmt", "--dir", &root, "--yaml", "--check"]);
}

/// A temp folder holding a copy of an old-format fixture's sops/.
fn old_repo(name: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&repo_root().join("tests/fixtures/old-format").join(name).join("sops"), &dir.path().join("sops"));
    let root = dir.path().join("sops").to_string_lossy().into_owned();
    (dir, root)
}

fn tree(root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for folder in ["", "bases", "instructions", "procedures", "agents"] {
        let Ok(entries) = std::fs::read_dir(root.join(folder)) else { continue };
        for e in entries {
            let p = e.unwrap().path();
            if p.is_file() {
                out.insert(p.strip_prefix(root).unwrap().to_string_lossy().into_owned(), read(&p));
            }
        }
    }
    out
}

#[test]
fn old_format_says_to_run_migrate() {
    let (dir, _) = old_repo("restaurants");
    let out = sopc_in(dir.path(), &["validate"], &[]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("sops/bases: error [old_format] the bases/ folder (now instructions/) is from the old format; run `sopc migrate` to convert this folder"), "{}", out.stderr);
    assert!(
        out.stderr.contains("sops/agents/tonys-pizza.yaml: error [old_format] `inherits` is from the old format"),
        "{}",
        out.stderr
    );
    assert!(!out.stderr.contains("[invalid_field]"), "only old_format errors: {}", out.stderr);
    assert_eq!(sopc_in(dir.path(), &[], &[]).code, 1);
}

#[test]
fn migrate_shows_the_plan_then_writes_with_yes() {
    let (dir, root) = old_repo("livekit-restaurant");
    let before = tree(Path::new(&root));
    let out = sopc_in(dir.path(), &["migrate"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    for line in [
        "Migrating sops to the current format (13 file(s) change):\n",
        "  sops/bases/closing.md → sops/instructions/closing.md: removed agents, position\n",
        "  sops/agents/tonys-pizza.yaml: instructions → context; removed inherits; blocks: restaurant-host, pizza-context, brand-voice, allergen-check, delivery-handling, closing\n",
        "  sops/sopc.yaml: removed sop_order\n",
        "Comments that go with removed fields:\n  sops/sopc.yaml\n    line 14: # SOPs listed here come first, in this order. Unlisted SOPs follow alphabetically.\n",
        "Checked: all 4 agent prompt(s) stay the same, except each agent's own text (now `context`) moves to the top.\n",
        "Nothing written. Rerun with --yes to write these changes.\n",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?} in:\n{}", out.stdout);
    }
    assert!(!out.stdout.contains("warning"), "locks are dropped without warnings:\n{}", out.stdout);
    assert_eq!(tree(Path::new(&root)), before, "a dry run writes nothing");

    let out = sopc_in(dir.path(), &["migrate", "--yes"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("Wrote 13 file(s). Verified on disk: 4 agent prompt(s) match.\n"), "{}", out.stdout);
    assert!(!Path::new(&root).join("bases").exists());
    let names = std::fs::read_dir(Path::new(&root).join("instructions")).unwrap();
    assert!(names.map(|e| e.unwrap().file_name()).all(|n| !n.to_string_lossy().starts_with('.')), "no temp files");
    let out = run_ok(dir.path(), &["validate"]);
    assert_eq!(out.stdout, "0 error(s), 0 warning(s)\n");
    run_ok(dir.path(), &["fmt", "--check"]);
    assert!(read(&Path::new(&root).join("agents/la-casita.yaml")).contains("  - brand-voice-es\n"));
    let out = run_ok(dir.path(), &["migrate"]);
    assert_eq!(out.stdout, "sops is already in the current format; nothing to migrate\n");
}

#[test]
fn migrated_prompts_match_the_old_build_except_the_context() {
    let (dir, root) = old_repo("restaurants");
    run_ok(dir.path(), &["migrate", "--yes"]);
    let new = built(&root, &dir.path().join("out"));
    let old = repo_root().join("tests/fixtures/old-format/restaurants/expected");
    for (name, text) in &new {
        let before = read(&old.join(name));
        if name.ends_with(".tool.json") {
            assert_eq!(text, &before, "{name}");
            continue;
        }
        let context = text.split("\n\n").next().unwrap();
        assert_eq!(text, &format!("{context}\n\n{}", before.replacen(&format!("\n\n{context}"), "", 1)), "{name}");
    }
    // The migrated folder is the fixture.
    assert_eq!(tree(Path::new(&root)), tree(&fixture().join("sops")));
}

#[test]
fn locked_says_to_run_migrate_which_removes_it() {
    let (dir, root) = repo();
    let voice = Path::new(&root).join("instructions/brand-voice.md");
    std::fs::write(&voice, format!("---\nlocked: true\n---\n{}", read(&voice))).unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    let out = sopc_in(dir.path(), &["validate"], &[]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "sops/instructions/brand-voice.md: error [removed_field] `locked` is no longer part of the format; run `sopc migrate` to remove it from this folder (or delete the line)\n"
    );
    let before = tree(Path::new(&root));
    let out = run_ok(dir.path(), &["migrate"]);
    assert_eq!(
        out.stdout,
        "Removing fields the format no longer has from sops (1 file(s) change):\n  sops/instructions/brand-voice.md: removed locked\nChecked: all 3 agent prompt(s) stay the same.\nNothing written. Rerun with --yes to write these changes.\n"
    );
    assert_eq!(tree(Path::new(&root)), before, "a dry run writes nothing");
    let out = run_ok(dir.path(), &["migrate", "--yes"]);
    assert!(out.stdout.contains("Wrote 1 file(s). Verified on disk: 3 agent prompt(s) match.\n"), "{}", out.stdout);
    assert_eq!(tree(Path::new(&root)), tree(&fixture().join("sops")));
    run_ok(dir.path(), &["validate"]);
    // The committed folder still has the lock; plan builds it as migrating would.
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert_eq!(out.stdout, "No agent prompts change.\n", "{}", out.stderr);
}

#[test]
fn plan_against_a_ref_from_before_migrating() {
    let (dir, root) = old_repo("restaurants");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
    run_ok(dir.path(), &["migrate", "--yes"]);
    // The old ref is built as migrating it would: no prompt changes.
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert_eq!(out.stdout, "No agent prompts change.\n", "{}", out.stderr);
    edit(&Path::new(&root).join("agents/sakura-sushi.yaml"), "  - reservations\n", "");
    let out = run(0, &["plan", "--dir", &root, "--against", "main", "--summary"]);
    assert!(out.stdout.contains("SOP `reservations` removed → 1 agent: sakura-sushi"), "{}", out.stdout);
}
