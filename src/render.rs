//! Assembling each agent's prompt, the get_sop payloads, and build/ (prompts and lock.json).

use crate::model::{Agent, Block, Sop, Step, Vars};
use crate::text::{pretty_json, sha256_hex};
use crate::workspace::{validate, Issue, Issues, Workspace};
use regex::{Captures, Regex};
use serde_json::{json, Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

static VARIABLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z_][\w.-]*)\s*\}\}").unwrap());

/// The placeholder names used in text.
pub fn find_variables(text: &str) -> BTreeSet<String> {
    VARIABLE.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// Replaces `{{name}}` with its value; names without a value are left as they are.
pub fn fill_variables(text: &str, values: &Vars) -> String {
    VARIABLE.replace_all(text, |c: &Captures| values.get(&c[1]).unwrap_or(&c[0]).to_string()).into_owned()
}

// --- resolution ----------------------------------------------------------------------------------

/// Where a block in an agent's expanded list came from: its own `blocks`, or a group.
fn source(group: &str) -> String {
    if group.is_empty() {
        "directly".into()
    } else {
        format!("in group `{group}`")
    }
}

/// Expands an agent's `blocks` (groups in place, recursively) into instruction and SOP ids, in
/// prompt order. Problems are reported against `path`: an id that is neither a block nor a group
/// (`unknown_block`) and a block reached twice (`duplicate_block`). Group cycles are reported by
/// [`crate::workspace::validate`]; here they are only stopped.
pub fn expand(ws: &Workspace, ids: &[String], path: &str, issues: &mut Vec<Issue>) -> Vec<String> {
    fn walk(
        ws: &Workspace,
        id: &str,
        group: &str,
        stack: &mut Vec<String>,
        out: &mut Vec<(String, String)>,
        path: &str,
        issues: &mut Vec<Issue>,
    ) {
        if let Some(members) = ws.config.group(id) {
            if stack.iter().any(|g| g == id) {
                return;
            }
            stack.push(id.to_string());
            for m in members {
                walk(ws, m, id, stack, out, path, issues);
            }
            stack.pop();
        } else if ws.block(id).is_none() {
            if group.is_empty() {
                let msg = format!("blocks lists '{id}', which is not an instruction, SOP or group");
                issues.push(Issue::error("unknown_block", path, msg));
            }
        } else if let Some((_, first)) = out.iter().find(|(b, _)| b == id) {
            let msg = format!(
                "'{id}' appears twice in blocks ({} and {}); list each block once",
                source(first),
                source(group)
            );
            issues.push(Issue::error("duplicate_block", path, msg));
        } else {
            out.push((id.to_string(), group.to_string()));
        }
    }
    let mut out = vec![];
    for id in ids {
        walk(ws, id, "", &mut vec![], &mut out, path, issues);
    }
    out.into_iter().map(|(id, _)| id).collect()
}

/// The blocks an agent gets, in prompt order.
pub fn resolve_blocks(ws: &Workspace, agent: &Agent) -> Vec<Block> {
    let ids = expand(ws, &agent.blocks, "", &mut vec![]);
    ids.iter().filter_map(|id| ws.block(id)).collect()
}

// --- SOP text ----------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum StepKind {
    Step,
    Forbidden,
    Warning,
}

pub fn render_step(s: &Step, kind: StepKind) -> String {
    let Some(tool) = s.tool() else { return s.text.trim().to_string() };
    let text = s.text.trim().trim_end_matches('.');
    if kind == StepKind::Forbidden {
        format!("{text}. This applies to the `{tool}` tool.")
    } else {
        format!("{text}. Use the `{tool}` tool.")
    }
}

/// An SOP's text; `steps`, `guards` (never/warning signs) and `details` (goal, guidance) can be left out.
pub fn render_sop(s: &Sop, steps: bool, guards: bool, details: bool) -> String {
    let mut lines = vec![format!("### {}", s.name)];
    if details && !s.description.is_empty() {
        lines.push(format!("Goal: {}", s.description.trim()));
    }
    if !s.scope.is_empty() {
        lines.push(format!("When this applies: {}", s.scope.trim()));
    }
    if details && !s.guidance.is_empty() {
        lines.extend([String::new(), s.guidance.trim().to_string()]);
    }
    if steps && !s.procedure_steps.is_empty() {
        lines.extend([String::new(), "Steps:".into()]);
        for (n, st) in s.procedure_steps.iter().enumerate() {
            lines.push(format!("{}. {}", n + 1, render_step(st, StepKind::Step)));
        }
    }
    for (show, title, list, kind) in [
        (guards, "Never:", &s.forbidden_actions, StepKind::Forbidden),
        (guards, "Warning signs:", &s.warning_signs, StepKind::Warning),
    ] {
        if show && !list.is_empty() {
            lines.extend([String::new(), title.into()]);
            lines.extend(list.iter().map(|st| format!("- {}", render_step(st, kind))));
        }
    }
    lines.join("\n")
}

/// What the prompt carries for an SOP, by delivery mode.
pub fn render_sop_in_prompt(s: &Sop) -> String {
    let fetch =
        format!("Before following this procedure, call the `get_sop` tool with id `{}` for the full steps.", s.id);
    match s.delivery.as_str() {
        "auto" => format!("{}\n\n{fetch}", render_sop(s, false, true, false)),
        "tool" => format!("{}\n\n{fetch}", render_sop(s, false, false, false)),
        _ => render_sop(s, true, true, true),
    }
}

fn steps_payload(steps: &[Step]) -> Json {
    let items = steps.iter().map(|s| {
        let mut o = Map::new();
        o.insert("text".into(), json!(s.text.trim()));
        if let Some(tool) = &s.tool {
            o.insert("tool".into(), json!(tool));
        }
        if s.required {
            o.insert("required".into(), json!(true));
        }
        Json::Object(o)
    });
    Json::Array(items.collect())
}

/// What get_sop serves for an SOP (before placeholders are filled). Text is trimmed, so
/// whitespace that YAML or Markdown adds around it doesn't reach the agent.
pub fn sop_payload(s: &Sop) -> Json {
    json!({
        "id": s.id, "name": s.name, "text": render_sop(s, true, true, true),
        "description": s.description.trim(), "scope": s.scope.trim(), "guidance": s.guidance.trim(),
        "procedureSteps": steps_payload(&s.procedure_steps),
        "forbiddenActions": steps_payload(&s.forbidden_actions),
        "warningSigns": steps_payload(&s.warning_signs),
    })
}

/// The tools an SOP's steps name, in order.
pub fn sop_tools(s: &Sop) -> impl Iterator<Item = &str> {
    s.procedure_steps.iter().chain(&s.forbidden_actions).chain(&s.warning_signs).filter_map(|st| st.tool())
}

pub fn fill_json(v: &mut Json, values: &Vars) {
    match v {
        Json::String(s) => *s = fill_variables(s, values),
        Json::Array(items) => items.iter_mut().for_each(|x| fill_json(x, values)),
        Json::Object(map) => map.values_mut().for_each(|x| fill_json(x, values)),
        _ => {}
    }
}

// --- rendering -----------------------------------------------------------------------------------

/// One agent's built prompt.
pub struct Rendered {
    pub agent: Agent,
    pub prompt: String,
    /// The instructions and SOPs it was built from, in prompt order.
    pub blocks: Vec<Block>,
    /// SOP id → get_sop payload, for SOPs not delivered entirely in the prompt.
    pub tool_payload: Map<String, Json>,
    pub tools: Vec<String>,
}

impl Rendered {
    pub fn hash(&self) -> String {
        sha256_hex(&self.prompt)
    }

    /// (kind, id, hash of the block's canonical JSON), in prompt order (agent first).
    pub fn block_hashes(&self) -> Vec<(String, String, String)> {
        let agent = ("agent".to_string(), self.agent.id.clone(), sha256_hex(&self.agent.canonical_json()));
        let blocks =
            self.blocks.iter().map(|b| (b.kind().to_string(), b.id().to_string(), sha256_hex(&b.canonical_json())));
        std::iter::once(agent).chain(blocks).collect()
    }

    /// Its SOPs, in prompt order.
    pub fn sops(&self) -> impl Iterator<Item = &Sop> {
        self.blocks.iter().filter_map(|b| match b {
            Block::Sop(s) => Some(s),
            Block::Instruction(_) => None,
        })
    }

    /// Ids of its instructions, in prompt order.
    pub fn instruction_ids(&self) -> Vec<&str> {
        self.blocks.iter().filter(|b| matches!(b, Block::Instruction(_))).map(|b| b.id()).collect()
    }
}

/// A rendered workspace, by agent id.
pub struct Build {
    pub agents: BTreeMap<String, Rendered>,
    pub warnings: Vec<Issue>,
}

impl Build {
    pub fn lock(&self) -> Json {
        let mut agents = Map::new();
        for (id, r) in &self.agents {
            let blocks: Vec<Json> = r
                .block_hashes()
                .into_iter()
                .map(|(kind, id, hash)| {
                    let mut entry = json!({"kind": kind, "id": id, "hash": hash});
                    if let Some(sop) = r.sops().find(|s| kind == "sop" && s.id == id) {
                        entry["rules"] = sop.rules().iter().map(|rule| rule.to_json()).collect();
                    }
                    entry
                })
                .collect();
            agents.insert(
                id.clone(),
                json!({"platform_ref": r.agent.platform_ref(), "hash": r.hash(), "blocks": blocks, "tools": r.tools}),
            );
        }
        json!({"version": 1, "agents": agents})
    }
}

/// Validates and renders every agent.
pub fn render_workspace(ws: &Workspace) -> Result<Build, Issues> {
    let (warnings, errors): (Vec<Issue>, Vec<Issue>) = validate(ws).into_iter().partition(|i| i.warning);
    if !errors.is_empty() {
        return Err(Issues(errors));
    }
    let agents = ws.agents.iter().map(|a| (a.id.clone(), render_agent(ws, a))).collect();
    Ok(Build { agents, warnings })
}

/// One agent's prompt: its `context`, then each block in list order. The SOP heading
/// (`sops_heading`, unless empty) goes once, right before the first SOP.
pub fn render_agent(ws: &Workspace, agent: &Agent) -> Rendered {
    let blocks = resolve_blocks(ws, agent);
    let mut sections = vec![agent.context.trim().to_string()];
    let mut heading = Some(ws.config.sops_heading.trim()).filter(|h| !h.is_empty());
    for block in &blocks {
        match block {
            Block::Instruction(i) => sections.push(i.text.clone()),
            Block::Sop(s) => match heading.take() {
                Some(h) => sections.push(format!("{h}\n\n{}", render_sop_in_prompt(s))),
                None => sections.push(render_sop_in_prompt(s)),
            },
        }
    }
    sections.retain(|s| !s.is_empty());
    let values = ws.config.variables.merged(&agent.variables);
    let prompt = fill_variables(&sections.join("\n\n"), &values) + "\n";

    let (mut tool_payload, mut tools) = (Map::new(), BTreeSet::new());
    for block in &blocks {
        let Block::Sop(s) = block else { continue };
        tools.extend(sop_tools(s).map(String::from));
        if s.delivery != "prompt" {
            let mut payload = sop_payload(s);
            fill_json(&mut payload, &values);
            tool_payload.insert(s.id.clone(), payload);
        }
    }
    Rendered { agent: agent.clone(), prompt, blocks, tool_payload, tools: tools.into_iter().collect() }
}

/// Writes <agent>.prompt.md, <agent>.tool.json and lock.json into `out`. The files are written
/// to a temp folder next to it and then renamed into place (lock.json last), so an error or
/// Ctrl-C never leaves a half-written file or a missing prompt. Files the previous build listed in
/// its lock.json that this one doesn't make are removed; nothing else in `out` is touched. A
/// non-empty `out` without lock.json is refused unless `force`. Returns the number of files written.
pub fn write_build(build: &Build, out: &Path, force: bool) -> anyhow::Result<usize> {
    let previous = previous_build(out, force)?;
    let mut files = vec![];
    for (id, r) in &build.agents {
        files.push((format!("{id}.prompt.md"), r.prompt.clone()));
        if !r.tool_payload.is_empty() {
            files.push((format!("{id}.tool.json"), pretty_json(&Json::Object(r.tool_payload.clone()), false) + "\n"));
        }
    }
    let lock = pretty_json(&build.lock(), false) + "\n";
    let abs = std::path::absolute(out)?;
    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        anyhow::bail!("can't write the build to {}", out.display())
    };
    let tmp = parent.join(format!(".{}.tmp", name.to_string_lossy()));
    let _ = std::fs::remove_dir_all(&tmp);
    let staged = std::fs::create_dir_all(&tmp)
        .and_then(|()| files.iter().try_for_each(|(name, text)| std::fs::write(tmp.join(name), text)))
        .and_then(|()| std::fs::write(tmp.join("lock.json"), &lock))
        .and_then(|()| std::fs::create_dir_all(out));
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(anyhow::Error::new(e).context(format!("can't write the build to {}", out.display())));
    }
    for (name, _) in &files {
        std::fs::rename(tmp.join(name), out.join(name))?;
    }
    for name in previous.iter().filter(|n| !files.iter().any(|(f, _)| f == *n)) {
        match std::fs::remove_file(out.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    std::fs::rename(tmp.join("lock.json"), out.join("lock.json"))?;
    std::fs::remove_dir(&tmp)?;
    Ok(files.len() + 1)
}

/// The prompt and tool files the build in `dir` made, from its lock.json. A folder that has files
/// but no lock.json wasn't built by sopc: an error unless `force` (then nothing is removed).
fn previous_build(dir: &Path, force: bool) -> anyhow::Result<Vec<String>> {
    let lock = std::fs::read_to_string(dir.join("lock.json")).ok().and_then(|t| serde_json::from_str::<Json>(&t).ok());
    let Some(lock) = lock else {
        let empty = std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none());
        if !empty && !force {
            anyhow::bail!(
                "{} has files but no lock.json, so sopc didn't build it; use an empty or new folder, \
                 or pass --force to write into it (only the files sopc builds are overwritten)",
                dir.display()
            );
        }
        return Ok(vec![]);
    };
    let ids = lock["agents"].as_object().into_iter().flat_map(|a| a.keys());
    Ok(ids.flat_map(|id| [format!("{id}.prompt.md"), format!("{id}.tool.json")]).collect())
}
