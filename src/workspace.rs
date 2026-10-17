//! Reading a sopc folder into a [`Workspace`], and the checks that need all of it.

use crate::model::{self, Block, Config, Instruction, Sop};
use crate::render::{expand, find_variables, sop_payload};
use crate::text::user_path;
use serde_yaml_ng::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

/// A validation error or warning.
#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub code: &'static str,
    pub message: String,
    pub path: String,
    pub warning: bool,
}

impl Issue {
    pub fn error(code: &'static str, path: &str, message: impl Into<String>) -> Self {
        Issue { code, message: message.into(), path: path.to_string(), warning: false }
    }
    pub fn warning(code: &'static str, path: &str, message: impl Into<String>) -> Self {
        Issue { warning: true, ..Issue::error(code, path, message) }
    }
    pub fn to_json(&self) -> serde_json::Value {
        let severity = if self.warning { "warning" } else { "error" };
        serde_json::json!({"code": self.code, "message": self.message, "path": self.path, "severity": severity})
    }
    /// The issue with its path from the current folder instead of from `root` (see [`user_path`]).
    pub fn seen_from_cwd(&self, root: &Path) -> Self {
        let path = if self.path.is_empty() { String::new() } else { user_path(root, &self.path) };
        Issue { path, ..self.clone() }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        let severity = if self.warning { "warning" } else { "error" };
        write!(f, "{severity} [{}] {}", self.code, self.message)
    }
}

/// Issues that stop a command.
#[derive(Debug)]
pub struct Issues(pub Vec<Issue>);

impl fmt::Display for Issues {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let lines: Vec<String> = self.0.iter().map(|i| i.to_string()).collect();
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for Issues {}

/// A loaded folder. Each list is in file-name order.
#[derive(Debug, Default)]
pub struct Workspace {
    pub config: Config,
    pub instructions: Vec<Instruction>,
    pub sops: Vec<Sop>,
    pub agents: Vec<model::Agent>,
    /// Warnings found while reading files (e.g. an empty Markdown section).
    pub warnings: Vec<Issue>,
}

impl Workspace {
    pub fn instruction(&self, id: &str) -> Option<&Instruction> {
        self.instructions.iter().find(|b| b.id == id)
    }
    pub fn sop(&self, id: &str) -> Option<&Sop> {
        self.sops.iter().find(|s| s.id == id)
    }
    /// The instruction or SOP with this id (an SOP wins if both exist, which is an error anyway).
    pub fn block(&self, id: &str) -> Option<Block> {
        let sop = self.sop(id).map(|s| Block::Sop(s.clone()));
        sop.or_else(|| self.instruction(id).map(|i| Block::Instruction(i.clone())))
    }
}

pub fn instruction_path(id: &str) -> String {
    format!("instructions/{id}.md")
}
pub fn agent_path(id: &str) -> String {
    format!("agents/{id}.yaml")
}

/// The file that marks the root of a sopc folder.
pub const CONFIG: &str = "sopc.yaml";
/// Its name before the project was renamed from OpenSOP (v0.0.5 and earlier). Read only to say
/// how to migrate, and so `--against` a ref from before the rename still builds.
pub const LEGACY_CONFIG: &str = "opensop.yaml";

/// The folder instructions were kept in before `sopc migrate`. Read only to say how to migrate,
/// and so `--against` a ref from before the change still builds.
pub const OLD_INSTRUCTIONS: &str = "bases";

/// Whether a path (relative to the root, with `/`) is a sopc source file.
pub fn is_source(rel: &str) -> bool {
    let (folder, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    rel == CONFIG
        || rel == LEGACY_CONFIG
        || ((folder == "instructions" || folder == OLD_INSTRUCTIONS) && name.ends_with(".md"))
        || (folder == "procedures" && (name.ends_with(".yaml") || name.ends_with(".md")))
        || (folder == "agents" && name.ends_with(".yaml"))
}

/// Text with \r\n and \r turned into \n.
pub fn universal_newlines(s: String) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s
    }
}

pub fn read_text(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(universal_newlines)
}

/// Writes a file through a temp file in the same folder and a rename, so it's never half-written.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The source files of a folder, keyed by path relative to it.
pub fn read_files(root: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for folder in ["", "instructions", OLD_INSTRUCTIONS, "procedures", "agents"] {
        let Ok(entries) = std::fs::read_dir(root.join(folder)) else { continue };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if folder.is_empty() { name } else { format!("{folder}/{name}") };
            if is_source(&rel) && entry.path().is_file() {
                files.insert(rel, read_text(&entry.path())?);
            }
        }
    }
    Ok(files)
}

pub fn load(root: &Path) -> anyhow::Result<Workspace> {
    Ok(load_files(&read_files(root)?)?)
}

/// The file name without its extension (`instructions/brand-voice.md` → `brand-voice`).
pub fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[..i],
        _ => name,
    }
}

/// The error for a file map without [`CONFIG`]; says to rename [`LEGACY_CONFIG`] if that is there.
pub fn missing_config(files: &BTreeMap<String, String>) -> Issues {
    let message = if files.contains_key(LEGACY_CONFIG) {
        format!("{CONFIG} not found, but {LEGACY_CONFIG} is: OpenSOP is now sopc; rename {LEGACY_CONFIG} to {CONFIG}")
    } else {
        format!("{CONFIG} not found")
    };
    Issues(vec![Issue::error("missing_config", "", message)])
}

/// The error for a field of the format before `sopc migrate`.
pub fn old_format(path: &str, what: &str) -> Issue {
    let msg = format!("{what} is from the old format; run `sopc migrate` to convert this folder");
    Issue::error("old_format", path, msg)
}

/// The error for a field the format no longer has (see [`model::REMOVED_FIELDS`]).
pub fn removed_field(path: &str, field: &str) -> Issue {
    let msg = format!(
        "`{field}` is no longer part of the format; run `sopc migrate` to remove it from this folder (or delete the line)"
    );
    Issue::error("removed_field", path, msg)
}

/// Reports and drops fields the format no longer has, so they aren't also "unknown".
pub fn removed_fields(map: &mut Mapping, path: &str, issues: &mut Vec<Issue>) {
    for field in model::REMOVED_FIELDS {
        if map.remove(*field).is_some() {
            issues.push(removed_field(path, field));
        }
    }
}

/// Reports fields of the old format in a file's mapping (or a Markdown file's front matter).
fn old_fields(map: &Mapping, fields: &[&str], path: &str, issues: &mut Vec<Issue>) {
    for field in fields.iter().filter(|f| map.contains_key(**f)) {
        issues.push(old_format(path, &format!("`{field}`")));
    }
}

/// Parses an in-memory file map (relative path → text). A folder in the format before
/// `sopc migrate` fails with `old_format` errors only.
pub fn load_files(files: &BTreeMap<String, String>) -> Result<Workspace, Issues> {
    let Some(config_text) = files.get(CONFIG) else {
        return Err(missing_config(files));
    };
    let mut issues = vec![];
    let mut ws = Workspace::default();
    if let Some(map) = load_yaml(config_text, CONFIG, &mut issues) {
        old_fields(&map, model::OLD_CONFIG_FIELDS, CONFIG, &mut issues);
        match model::parse_config(&map) {
            Ok(config) => ws.config = config,
            Err(errs) => field_errors(errs, CONFIG, &mut issues),
        }
    }
    let in_folder = |folder: &'static str| {
        files.iter().filter(move |(p, _)| p.starts_with(folder) && p[folder.len()..].starts_with('/'))
    };
    if in_folder(OLD_INSTRUCTIONS).next().is_some() {
        issues.push(old_format(OLD_INSTRUCTIONS, "the bases/ folder (now instructions/)"));
    }
    for (path, text) in in_folder("instructions") {
        let (meta, body) = split_front_matter(text);
        let Some(mut map) = load_yaml(meta, path, &mut issues) else { continue };
        old_fields(&map, model::OLD_INSTRUCTION_FIELDS, path, &mut issues);
        removed_fields(&mut map, path, &mut issues);
        set_id(&mut map, path, &mut issues);
        map.insert("text".into(), body.trim().into());
        match model::parse_instruction(&map) {
            Ok(instruction) => ws.instructions.push(instruction),
            Err(errs) => field_errors(errs, path, &mut issues),
        }
    }
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (path, text) in in_folder("procedures") {
        if let Some(other) = seen.insert(stem(path), path) {
            let msg = format!("{other} and {path} are the same SOP '{}'; keep one", stem(path));
            issues.push(Issue::error("duplicate_file", path, msg));
            continue;
        }
        match parse_sop_file(path, text) {
            Ok((sop, warnings)) => {
                ws.sops.push(sop);
                ws.warnings.extend(warnings);
            }
            Err(errs) => issues.extend(errs),
        }
    }
    for (path, text) in in_folder("agents") {
        let Some(mut map) = load_yaml(text, path, &mut issues) else { continue };
        old_fields(&map, model::OLD_AGENT_FIELDS, path, &mut issues);
        set_id(&mut map, path, &mut issues);
        match model::parse_agent(&map) {
            Ok(agent) => ws.agents.push(agent),
            Err(errs) => field_errors(errs, path, &mut issues),
        }
    }
    if issues.iter().any(|i| i.code == "old_format") {
        // The rest follows from the old format (e.g. "unknown field"); migrating fixes it.
        issues.retain(|i| i.code == "old_format");
    }
    if issues.is_empty() {
        Ok(ws)
    } else {
        Err(Issues(issues))
    }
}

/// Reads one SOP file, YAML or Markdown, on its own. Ok carries the SOP and its warnings.
pub fn parse_sop_file(path: &str, text: &str) -> Result<(Sop, Vec<Issue>), Vec<Issue>> {
    let (mut issues, mut warnings) = (vec![], vec![]);
    let markdown = path.ends_with(".md");
    let map = if markdown {
        crate::sopfile::parse_markdown(text, path, &mut issues, &mut warnings)
    } else {
        load_yaml(text, path, &mut issues).filter(|map| check_steps(map, path, &mut issues))
    };
    let Some(mut map) = map else { return Err(issues) };
    if !markdown {
        old_fields(&map, model::OLD_SOP_FIELDS, path, &mut issues);
        removed_fields(&mut map, path, &mut issues);
    }
    set_id(&mut map, path, &mut issues);
    let no_steps = match map.get("procedureSteps") {
        None | Some(Value::Null) => true,
        Some(Value::Sequence(items)) => items.is_empty(),
        _ => false,
    };
    if no_steps && !markdown {
        let line = text.lines().position(|l| l.starts_with("procedureSteps:"));
        let at = line.map(|n| format!("line {}: ", n + 1)).unwrap_or_default();
        let msg = format!("{at}no steps; every SOP needs at least one entry in procedureSteps");
        issues.push(Issue::error("missing_steps", path, msg));
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    match model::parse_sop(&map) {
        Ok(sop) => Ok((Sop { file: path.to_string(), ..sop }, warnings)),
        Err(errs) => {
            field_errors(errs, path, &mut issues);
            Err(issues)
        }
    }
}

fn field_errors(errs: Vec<String>, path: &str, issues: &mut Vec<Issue>) {
    issues.extend(errs.into_iter().map(|e| Issue::error("invalid_field", path, e)));
}

/// Splits `---` front matter from the body.
pub fn split_front_matter(text: &str) -> (&str, &str) {
    if text.starts_with("---\n") {
        if let Some(i) = text[3..].find("\n---") {
            let end = i + 3;
            let meta = if end > 4 { &text[4..end] } else { "" };
            return (meta, text[end + 4..].trim_start_matches('\n'));
        }
    }
    ("", text)
}

const COLON_HINT: &str =
    " Usually a colon followed by a space inside unquoted text: put the text in quotes, or use a | block.";

/// Parses a YAML mapping, or reports the problem. An empty file is an empty mapping.
pub fn load_yaml(text: &str, path: &str, issues: &mut Vec<Issue>) -> Option<Mapping> {
    match serde_yaml_ng::from_str::<Value>(text) {
        Err(err) => {
            let mut msg = err.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
            if msg.contains("mapping values are not allowed") {
                msg.push_str(COLON_HINT);
            }
            issues.push(Issue::error("invalid_yaml", path, msg));
            None
        }
        Ok(Value::Mapping(map)) => Some(map),
        Ok(Value::Null) => Some(Mapping::new()),
        Ok(_) => {
            issues.push(Issue::error("invalid_yaml", path, "expected a mapping"));
            None
        }
    }
}

/// Sets "id" from the file name; an explicit id must agree.
fn set_id(map: &mut Mapping, path: &str, issues: &mut Vec<Issue>) {
    let id = stem(path);
    if let Some(v) = map.get("id") {
        if v.as_str() != Some(id) {
            let msg = format!("id '{}' does not match file name '{id}'", model::scalar_text(v));
            issues.push(Issue::error("id_mismatch", path, msg));
        }
    }
    map.insert("id".into(), id.into());
}

/// Catches steps YAML turned into something other than text.
fn check_steps(map: &Mapping, path: &str, issues: &mut Vec<Issue>) -> bool {
    let before = issues.len();
    for field in ["procedureSteps", "forbiddenActions", "warningSigns"] {
        let Some(Value::Sequence(items)) = map.get(field) else { continue };
        for (i, item) in items.iter().enumerate() {
            let at = format!("{field}[{i}]");
            match item {
                Value::Mapping(m)
                    if m.len() == 1
                        && !m.keys().all(|k| k.as_str().is_some_and(|k| model::STEP_FIELDS.contains(&k))) =>
                {
                    let (k, v) = m.iter().next().unwrap();
                    let text = match v {
                        Value::Null => format!("{}:", model::scalar_text(k)),
                        v => format!("{}: {}", model::scalar_text(k), model::scalar_text(v)),
                    };
                    let quoted = crate::text::ascii_json(&serde_json::to_string(&text).unwrap());
                    let msg = format!("{at} was read as a key and value because of the colon. Put the whole step in quotes: - {quoted}");
                    issues.push(Issue::error("colon_in_step", path, msg));
                }
                Value::Null => issues.push(Issue::error("empty_step", path, format!("{at} is empty"))),
                Value::Bool(_) | Value::Number(_) => {
                    let msg =
                        format!("{at} was read as {}, not text. Put the step in quotes.", model::scalar_text(item));
                    issues.push(Issue::error("unquoted_value", path, msg));
                }
                _ => {}
            }
        }
    }
    issues.len() == before
}

// --- export ------------------------------------------------------------------------------------

/// Every block's parsed fields, for `sopc export`: each block's canonical JSON (the fields its
/// lock.json hash is taken over) plus the file it was read from, and sopc.yaml's settings. SOPs
/// also get `rules`, as in lock.json.
pub fn export(ws: &Workspace) -> serde_json::Value {
    fn with_file(canonical: String, file: String) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(&canonical).expect("canonical JSON parses");
        v.as_object_mut().unwrap().insert("file".into(), file.into());
        v
    }
    let vars: serde_json::Map<String, serde_json::Value> =
        ws.config.variables.0.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect();
    let groups: serde_json::Map<String, serde_json::Value> =
        ws.config.groups.iter().map(|(k, v)| (k.clone(), serde_json::json!(v))).collect();
    let agents = ws.agents.iter().map(|a| {
        let mut v = with_file(a.canonical_json(), agent_path(&a.id));
        let obj = v.as_object_mut().unwrap();
        obj.insert("platform".into(), a.platform().into());
        obj.insert("platform_id".into(), a.platform_id().into());
        v
    });
    let instructions = ws.instructions.iter().map(|i| with_file(i.canonical_json(), instruction_path(&i.id)));
    let sops = ws.sops.iter().map(|s| {
        let mut v = with_file(s.canonical_json(), s.file.clone());
        v["rules"] = s.rules().iter().map(|r| r.to_json()).collect();
        v
    });
    serde_json::json!({
        "config": {"variables": vars, "groups": groups, "sops_heading": ws.config.sops_heading},
        "instructions": instructions.collect::<Vec<_>>(),
        "sops": sops.collect::<Vec<_>>(),
        "agents": agents.collect::<Vec<_>>(),
    })
}

// --- whole-workspace checks ----------------------------------------------------------------------

/// Ids, groups and variables.
pub fn validate(ws: &Workspace) -> Vec<Issue> {
    let mut issues = ws.warnings.clone();

    let instruction_ids: BTreeSet<&str> = ws.instructions.iter().map(|b| b.id.as_str()).collect();
    for sop in ws.sops.iter().filter(|s| instruction_ids.contains(s.id.as_str())) {
        let msg = format!("'{}' is both an instruction and an SOP; ids must be unique", sop.id);
        issues.push(Issue::error("duplicate_id", "", msg));
    }
    for (name, _) in ws.config.groups.iter().filter(|(n, _)| ws.block(n).is_some()) {
        let msg = format!("group '{name}' has the same name as an instruction or SOP; names must be unique");
        issues.push(Issue::error("duplicate_id", CONFIG, msg));
    }

    let mut seen_refs: BTreeMap<String, &str> = BTreeMap::new();
    for a in &ws.agents {
        let r = a.platform_ref();
        if let Some(prev) = seen_refs.get(&r) {
            issues.push(Issue::error(
                "duplicate_platform_ref",
                &agent_path(&a.id),
                format!("{r} is used by '{prev}' and '{}'", a.id),
            ));
        }
        seen_refs.insert(r, &a.id);
    }

    for (name, members) in &ws.config.groups {
        for id in members.iter().filter(|id| ws.block(id).is_none() && ws.config.group(id).is_none()) {
            let msg = format!("group '{name}' lists '{id}', which is not an instruction, SOP or group");
            issues.push(Issue::error("unknown_block", CONFIG, msg));
        }
    }
    issues.extend(group_cycles(ws));

    for s in &ws.sops {
        let rules = s.rules();
        for (n, rule) in rules.iter().enumerate() {
            if let Some(first) = rules[..n].iter().find(|r| r.id == rule.id) {
                let msg = format!(
                    "{} {} and {} {} have the same text; remove one",
                    rule_label(rule.kind),
                    first.index,
                    rule_label(rule.kind),
                    rule.index
                );
                issues.push(Issue::error("duplicate_rule", &s.file, msg));
            }
        }
    }

    for s in ws.sops.iter().filter(|s| s.description.trim().is_empty()) {
        issues.push(Issue::warning(
            "missing_goal",
            &s.file,
            "no description (goal); QA can't judge whether the goal was met",
        ));
    }

    if issues.iter().any(|i| !i.warning && i.path == CONFIG) {
        return issues; // expanding broken groups would be misleading
    }
    let mut used = BTreeSet::new();
    for agent in &ws.agents {
        let path = agent_path(&agent.id);
        let before = issues.len();
        let ids = expand(ws, &agent.blocks, &path, &mut issues);
        used.extend(ids.iter().cloned());
        if issues.len() > before {
            continue;
        }
        let mut names = find_variables(&agent.context);
        for block in ids.iter().filter_map(|id| ws.block(id)) {
            match block {
                Block::Instruction(i) => names.extend(find_variables(&i.text)),
                Block::Sop(s) => for_each_string(&sop_payload(&s), &mut |text| names.extend(find_variables(text))),
            }
        }
        let values = ws.config.variables.merged(&agent.variables);
        for name in names.iter().filter(|n| values.get(n).is_none()) {
            issues.push(Issue::error("unset_variable", &path, format!("'{{{{{name}}}}}' is used but has no value")));
        }
    }
    if !ws.agents.is_empty() {
        let files = ws.instructions.iter().map(|i| (&i.id, instruction_path(&i.id), "instruction"));
        for (id, path, kind) in files.chain(ws.sops.iter().map(|s| (&s.id, s.file.clone(), "SOP"))) {
            if !used.contains(id) {
                let msg =
                    format!("no agent uses this {kind}; add '{id}' to an agent's blocks or a group, or delete it");
                issues.push(Issue::warning("unused_block", &path, msg));
            }
        }
    }
    issues
}

/// How a rule kind is named in messages.
fn rule_label(kind: &str) -> &'static str {
    match kind {
        "step" => "step",
        "never" => "never",
        _ => "warning sign",
    }
}

pub fn for_each_string(v: &serde_json::Value, f: &mut dyn FnMut(&str)) {
    match v {
        serde_json::Value::String(s) => f(s),
        serde_json::Value::Array(items) => items.iter().for_each(|x| for_each_string(x, f)),
        serde_json::Value::Object(map) => map.values().for_each(|x| for_each_string(x, f)),
        _ => {}
    }
}

/// Groups that contain themselves, directly or through other groups.
fn group_cycles(ws: &Workspace) -> Vec<Issue> {
    fn walk(
        ws: &Workspace,
        id: &str,
        stack: &mut Vec<String>,
        reported: &mut BTreeSet<BTreeSet<String>>,
        out: &mut Vec<Issue>,
    ) {
        if let Some(i) = stack.iter().position(|s| s == id) {
            let cycle = &stack[i..];
            if reported.insert(cycle.iter().cloned().collect()) {
                let message = format!("groups contain each other in a loop: {} → {id}", cycle.join(" → "));
                out.push(Issue::error("group_cycle", CONFIG, message));
            }
            return;
        }
        if let Some(members) = ws.config.group(id) {
            stack.push(id.to_string());
            for m in members {
                walk(ws, m, stack, reported, out);
            }
            stack.pop();
        }
    }
    let (mut reported, mut out) = (BTreeSet::new(), vec![]);
    for (name, _) in &ws.config.groups {
        walk(ws, name, &mut vec![], &mut reported, &mut out);
    }
    out
}
