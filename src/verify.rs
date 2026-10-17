//! `sopc verify`: fetches each agent's live prompt from its platform and compares it with the
//! compiled one, to catch hotfixes made in a platform's dashboard that git doesn't have.
//!
//! It also checks that every tool the agent's SOPs name exists on the live agent, reading the
//! tool list from the same responses where the platform includes it.
//!
//! Read-only: only GET requests are sent. Base URLs can be overridden for tests and proxies
//! (`SOPC_ELEVENLABS_URL`, `SOPC_VAPI_URL`, `SOPC_RETELL_URL`).

use crate::plan::labelled_diff;
use crate::render::Build;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Requests in flight at once.
const WORKERS: usize = 8;

/// Where a platform's API is and how to authenticate with it.
struct Api {
    name: &'static str,
    key_var: &'static str,
    url_var: &'static str,
    default_url: &'static str,
}

const ELEVENLABS: Api = Api {
    name: "ElevenLabs",
    key_var: "ELEVENLABS_API_KEY",
    url_var: "SOPC_ELEVENLABS_URL",
    default_url: "https://api.elevenlabs.io",
};
const VAPI: Api =
    Api { name: "Vapi", key_var: "VAPI_API_KEY", url_var: "SOPC_VAPI_URL", default_url: "https://api.vapi.ai" };
const RETELL: Api = Api {
    name: "Retell",
    key_var: "RETELL_API_KEY",
    url_var: "SOPC_RETELL_URL",
    default_url: "https://api.retellai.com",
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    InSync,
    Drifted,
    NotComparable,
    Skipped,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::InSync => "in_sync",
            Status::Drifted => "drifted",
            Status::NotComparable => "not_comparable",
            Status::Skipped => "skipped",
            Status::Error => "error",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Status::InSync => "in sync",
            Status::Drifted => "drifted",
            Status::NotComparable => "not comparable",
            Status::Skipped => "skipped",
            Status::Error => "error",
        }
    }
}

/// One agent's result.
pub struct Check {
    pub id: String,
    pub platform: String,
    pub platform_id: String,
    pub status: Status,
    pub reason: Option<String>,
    /// Compiled → live, when drifted.
    pub diff: Option<String>,
    /// The tool check; `None` when the agent couldn't be read.
    pub tools: Option<Tools>,
}

/// A tool an agent's SOPs name.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolRef {
    pub name: String,
    /// Named by a `required` step (or `get_sop`, which the prompt tells the agent to call).
    pub required: bool,
}

/// A referenced tool the live agent doesn't have.
#[derive(Clone, Debug, PartialEq)]
pub struct Missing {
    pub tool: ToolRef,
    /// A live tool with a near-identical name.
    pub suggestion: Option<String>,
}

/// The result of comparing an agent's referenced tools with its live ones.
#[derive(Debug, Default, PartialEq)]
pub struct Tools {
    pub referenced: Vec<ToolRef>,
    /// Referenced but not on the platform: fails verify.
    pub missing: Vec<Missing>,
    /// On the platform but named by no SOP: information only.
    pub unreferenced: Vec<String>,
    /// Referenced and not listed, but the agent has tools sopc can't list (MCP servers, shared
    /// tools), so they may exist: doesn't fail.
    pub unconfirmed: Vec<String>,
    /// Why some tools can't be listed, or why tools weren't checked at all.
    pub note: Option<String>,
    /// The live tools couldn't be read: fails verify.
    pub error: Option<String>,
    /// False when nothing was compared (LiveKit, a custom LLM).
    pub checked: bool,
}

impl Tools {
    fn failed(&self) -> bool {
        !self.missing.is_empty() || self.error.is_some()
    }

    fn to_json(&self) -> Json {
        let refs = |r: &ToolRef| json!({"name": r.name, "required": r.required});
        let missing =
            |m: &Missing| json!({"name": m.tool.name, "required": m.tool.required, "suggestion": m.suggestion});
        json!({
            "checked": self.checked,
            "referenced": self.referenced.iter().map(refs).collect::<Vec<_>>(),
            "missing": self.missing.iter().map(missing).collect::<Vec<_>>(),
            "unreferenced": self.unreferenced, "unconfirmed": self.unconfirmed,
            "note": self.note, "error": self.error,
        })
    }

    /// Indented lines under the agent's line; none when everything matches.
    fn lines(&self) -> Vec<String> {
        let mut out = vec![];
        if let Some(e) = &self.error {
            out.push(format!("  tools: error: {e}"));
        }
        if !self.checked {
            if let Some(note) = &self.note {
                out.push(format!("  tools: not checked: {note}"));
            }
            return out;
        }
        for m in &self.missing {
            let required = if m.tool.required { " (required step)" } else { "" };
            let hint = m.suggestion.as_ref().map(|s| format!("; did you mean {s}?")).unwrap_or_default();
            out.push(format!("  tools: missing {}{required}{hint}", m.tool.name));
        }
        if !self.unconfirmed.is_empty() {
            let why = self.note.as_deref().unwrap_or("some tools can't be listed");
            out.push(format!("  tools: not confirmed: {} ({why})", self.unconfirmed.join(", ")));
        }
        if !self.unreferenced.is_empty() {
            out.push(format!("  tools: not in any SOP: {}", self.unreferenced.join(", ")));
        }
        out
    }
}

/// The tools a live agent has, by the name the model sees.
#[derive(Debug, Default, PartialEq)]
pub struct LiveTools {
    pub names: BTreeSet<String>,
    /// Tool ids still to be looked up by name (ElevenLabs workspace tools, Vapi saved tools).
    pub ids: BTreeSet<String>,
    /// Set when the agent also has tools sopc can't list (why).
    pub opaque: Option<String>,
}

/// What `verify` read for an agent's tools.
#[derive(Debug, PartialEq)]
pub enum ToolList {
    Live(LiveTools),
    /// Tools aren't on the platform (why).
    NotChecked(String),
    /// They couldn't be read.
    Error(String),
}

/// What a platform returned for an agent.
struct Fetched {
    live: Live,
    /// A note for the agent's line, e.g. that a Retell draft was compared.
    note: Option<String>,
    tools: ToolList,
}

/// What a platform holds for an agent.
#[derive(Debug, PartialEq)]
pub enum Live {
    Prompt(String),
    /// The agent's prompt can't be compared as one text (why).
    NotComparable(String),
}

pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|c| matches!(c.status, Status::Drifted | Status::Error))
            && !self.checks.iter().any(|c| c.tools.as_ref().is_some_and(Tools::failed))
    }

    /// Agents with missing tools, and agents whose tools couldn't be read.
    fn tool_counts(&self) -> (usize, usize) {
        let tools = || self.checks.iter().filter_map(|c| c.tools.as_ref());
        (tools().filter(|t| !t.missing.is_empty()).count(), tools().filter(|t| t.error.is_some()).count())
    }

    fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    /// e.g. "4 in sync, 1 drifted, 1 skipped".
    pub fn summary(&self) -> String {
        let all = [Status::InSync, Status::Drifted, Status::NotComparable, Status::Skipped, Status::Error];
        let mut parts: Vec<String> = all
            .into_iter()
            .filter(|s| self.count(*s) > 0)
            .map(|s| {
                let n = self.count(s);
                if s == Status::Error && n > 1 {
                    format!("{n} errors")
                } else {
                    format!("{n} {}", s.label())
                }
            })
            .collect();
        let (missing, errors) = self.tool_counts();
        if missing > 0 {
            parts.push(format!("{missing} with missing tools"));
        }
        if errors > 0 {
            parts.push(format!("{errors} tool {}", if errors == 1 { "error" } else { "errors" }));
        }
        if parts.is_empty() {
            "no agents to verify".into()
        } else {
            parts.join(", ")
        }
    }

    pub fn text(&self) -> String {
        let mut out = vec![];
        for c in &self.checks {
            let reason = c.reason.as_ref().map(|r| format!(": {r}")).unwrap_or_default();
            out.push(format!("{:<24} {:<11} {:<28} {}{reason}", c.id, c.platform, c.platform_id, c.status.label()));
            out.extend(c.tools.iter().flat_map(Tools::lines));
        }
        for c in self.checks.iter().filter(|c| c.diff.is_some()) {
            out.extend([String::new(), c.diff.as_deref().unwrap_or_default().trim_end().to_string()]);
        }
        out.extend([String::new(), self.summary()]);
        out.join("\n") + "\n"
    }

    pub fn to_json(&self) -> Json {
        let agents: Vec<Json> = self
            .checks
            .iter()
            .map(|c| {
                json!({
                    "id": c.id, "platform": c.platform, "platform_id": c.platform_id,
                    "platform_ref": format!("{}:{}", c.platform, c.platform_id),
                    "status": c.status.as_str(), "reason": c.reason, "diff": c.diff,
                    "tools": c.tools.as_ref().map(Tools::to_json),
                })
            })
            .collect();
        let (missing_tools, tool_errors) = self.tool_counts();
        let summary = json!({
            "in_sync": self.count(Status::InSync), "drifted": self.count(Status::Drifted),
            "not_comparable": self.count(Status::NotComparable), "skipped": self.count(Status::Skipped),
            "error": self.count(Status::Error), "missing_tools": missing_tools, "tool_errors": tool_errors,
        });
        json!({"ok": self.ok(), "summary": summary, "agents": agents})
    }
}

/// Line endings made `\n` and trailing whitespace at the end removed; one final newline.
pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").trim_end().to_string() + "\n"
}

/// Checks `ids` (every agent when empty), several at a time; results keep the order of `ids`.
pub fn verify(build: &Build, ids: &[String]) -> Report {
    let ids: Vec<&String> = if ids.is_empty() { build.agents.keys().collect() } else { ids.iter().collect() };
    let http = client();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Check>>> = Mutex::new((0..ids.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..WORKERS.min(ids.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(id) = ids.get(i) else { break };
                let check = check(&http, build, id);
                results.lock().unwrap()[i] = Some(check);
            });
        }
    });
    Report { checks: results.into_inner().unwrap().into_iter().flatten().collect() }
}

fn check(http: &ureq::Agent, build: &Build, id: &str) -> Check {
    let r = &build.agents[id];
    let (platform, platform_id) = (r.agent.platform(), r.agent.platform_id());
    let mut check = Check {
        id: id.to_string(),
        platform: platform.to_string(),
        platform_id: platform_id.to_string(),
        status: Status::Error,
        reason: None,
        diff: None,
        tools: None,
    };
    let fetched = match platform {
        "elevenlabs" => fetch_elevenlabs(http, platform_id),
        "vapi" => fetch_vapi(http, platform_id),
        "retell" => fetch_retell(http, platform_id),
        _ => {
            check.status = Status::Skipped;
            check.reason = Some("LiveKit: your code loads the prompt and tools; nothing to fetch".into());
            return check;
        }
    };
    let fetched = match fetched {
        Ok(f) => f,
        Err(e) => {
            check.reason = Some(e);
            return check;
        }
    };
    check.tools = Some(compare_tools(referenced_tools(r), fetched.tools));
    match fetched.live {
        Live::NotComparable(why) => {
            check.status = Status::NotComparable;
            check.reason = Some(fetched.note.map_or(why.clone(), |note| format!("{why}; {note}")));
        }
        Live::Prompt(live) => {
            check.reason = fetched.note;
            let (compiled, live) = (normalize(&r.prompt), normalize(&live));
            if compiled == live {
                check.status = Status::InSync;
            } else {
                check.status = Status::Drifted;
                let old = format!("compiled/{id}.prompt.md");
                let new = format!("live/{}", r.agent.platform_ref());
                check.diff = Some(labelled_diff(&compiled, &live, &old, &new));
            }
        }
    }
    check
}

// --- tools -----------------------------------------------------------------------------------

/// The tools an agent's compiled blocks name (steps, nevers and warning signs, groups expanded),
/// by name, marked `required` when any `required` step names it. `get_sop` is included when an
/// SOP is delivered by tool, since the prompt tells the agent to call it.
pub fn referenced_tools(r: &crate::render::Rendered) -> Vec<ToolRef> {
    let mut tools: BTreeMap<String, bool> = BTreeMap::new();
    for sop in r.sops() {
        for step in sop.procedure_steps.iter().chain(&sop.forbidden_actions).chain(&sop.warning_signs) {
            if let Some(tool) = step.tool() {
                *tools.entry(tool.to_string()).or_default() |= step.required;
            }
        }
    }
    if !r.tool_payload.is_empty() {
        tools.insert("get_sop".into(), true);
    }
    tools.into_iter().map(|(name, required)| ToolRef { name, required }).collect()
}

/// Compares the referenced tools with what the platform has.
pub fn compare_tools(referenced: Vec<ToolRef>, live: ToolList) -> Tools {
    let live = match live {
        ToolList::Live(live) => live,
        ToolList::NotChecked(why) => return Tools { referenced, note: Some(why), ..Tools::default() },
        ToolList::Error(e) => return Tools { referenced, error: Some(e), ..Tools::default() },
    };
    let mut tools = Tools { checked: true, note: live.opaque.clone(), ..Tools::default() };
    for r in &referenced {
        if live.names.contains(&r.name) {
            continue;
        }
        if live.opaque.is_some() {
            tools.unconfirmed.push(r.name.clone());
        } else {
            tools.missing.push(Missing { tool: r.clone(), suggestion: suggest(&r.name, &live.names) });
        }
    }
    tools.unreferenced = live.names.iter().filter(|n| !referenced.iter().any(|r| &&r.name == n)).cloned().collect();
    tools.referenced = referenced;
    tools
}

/// The live name closest to `name`: the same ignoring case, `_` and `-`, or within 2 edits.
pub fn suggest(name: &str, live: &BTreeSet<String>) -> Option<String> {
    let squash = |s: &str| s.chars().filter(|c| !matches!(c, '_' | '-')).collect::<String>().to_lowercase();
    if let Some(same) = live.iter().find(|l| squash(l) == squash(name)) {
        return Some(same.clone());
    }
    let distances = live.iter().map(|l| (edit_distance(&l.to_lowercase(), &name.to_lowercase()), l));
    distances.filter(|(d, _)| *d <= 2).min_by_key(|(d, _)| *d).map(|(_, l)| l.clone())
}

/// Levenshtein distance, by characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Looks up the names of `live.ids` with `lookup`; the first failure is the result.
fn resolve_ids(
    mut live: LiveTools,
    mut lookup: impl FnMut(&str) -> Result<Json, String>,
    name: fn(&Json) -> Tool,
) -> ToolList {
    for id in std::mem::take(&mut live.ids) {
        match lookup(&id) {
            Ok(tool) => live.add(name(&tool)),
            Err(e) => return ToolList::Error(format!("can't read tool {id}: {e}")),
        }
    }
    ToolList::Live(live)
}

/// One tool definition: its name, or that it's an MCP server whose tools can't be listed.
pub enum Tool {
    Named(String),
    Mcp,
    Unknown,
}

impl LiveTools {
    fn add(&mut self, tool: Tool) {
        match tool {
            Tool::Named(name) => {
                self.names.insert(name);
            }
            Tool::Mcp => self.opaque = Some(MCP.into()),
            Tool::Unknown => {}
        }
    }
}

const MCP: &str = "the agent uses MCP servers, whose tools sopc can't list";

/// A non-empty string field.
fn str_of<'a>(v: &'a Json, key: &str) -> Option<&'a str> {
    v[key].as_str().filter(|s| !s.trim().is_empty())
}

fn has_items(v: &Json) -> bool {
    v.as_array().is_some_and(|a| !a.is_empty())
}

// --- fetching --------------------------------------------------------------------------------

fn fetch_elevenlabs(http: &ureq::Agent, id: &str) -> Result<Fetched, String> {
    let agent = get(http, &ELEVENLABS, &format!("/v1/convai/agents/{}", segment(id)), "xi-api-key", "")?;
    let lookup =
        |tool_id: &str| get(http, &ELEVENLABS, &format!("/v1/convai/tools/{}", segment(tool_id)), "xi-api-key", "");
    let tools = resolve_ids(elevenlabs_tools(&agent), lookup, elevenlabs_tool);
    Ok(Fetched { live: elevenlabs_prompt(&agent), note: None, tools })
}

fn fetch_vapi(http: &ureq::Agent, id: &str) -> Result<Fetched, String> {
    let assistant = get(http, &VAPI, &format!("/assistant/{}", segment(id)), "Authorization", "Bearer ")?;
    let lookup = |tool_id: &str| get(http, &VAPI, &format!("/tool/{}", segment(tool_id)), "Authorization", "Bearer ");
    let tools = resolve_ids(vapi_tools(&assistant), lookup, vapi_tool);
    Ok(Fetched { live: vapi_prompt(&assistant), note: None, tools })
}

/// Retell keeps every version of an agent; `get-agent` without a version returns the newest,
/// which may be a draft. Read the newest published version instead (Retell's
/// `latest_published`), or the newest draft, with a note, when nothing is published.
fn fetch_retell(http: &ureq::Agent, id: &str) -> Result<Fetched, String> {
    let (path, note) = match retell_published_version(http, id)? {
        Some(v) => (format!("/get-agent/{}?version={v}", segment(id)), None),
        None => (format!("/get-agent/{}", segment(id)), Some(RETELL_UNPUBLISHED.to_string())),
    };
    let agent = get(http, &RETELL, &path, "Authorization", "Bearer ")?;
    let (live, tools) = retell_agent_prompt(http, &agent)?;
    Ok(Fetched { live, note, tools })
}

const RETELL_UNPUBLISHED: &str = "not published on Retell; compared the latest draft";

/// The newest published version of a Retell agent, from its version list (newest first).
fn retell_published_version(http: &ureq::Agent, id: &str) -> Result<Option<u64>, String> {
    let mut page = String::new();
    loop {
        let path = format!("/list-agent-versions/{}?limit=1000&sort_order=descending{page}", segment(id));
        let list = get(http, &RETELL, &path, "Authorization", "Bearer ")?;
        let published = list["items"].as_array().into_iter().flatten().filter(|v| v["is_published"] == true);
        if let Some(version) = published.filter_map(|v| v["version"].as_u64()).max() {
            return Ok(Some(version));
        }
        let key = list["pagination_key"].as_str().filter(|k| !k.is_empty() && list["has_more"] == true);
        // Stop at the last page, or at a page that points to itself.
        let Some(next) = key.map(|k| format!("&pagination_key={}", segment(k))).filter(|n| *n != page) else {
            return Ok(None);
        };
        page = next;
    }
}

/// The prompt and tools of the LLM (or conversation flow) a Retell agent version uses, at the
/// version that agent version is pinned to.
fn retell_agent_prompt(http: &ureq::Agent, agent: &Json) -> Result<(Live, ToolList), String> {
    let engine = &agent["response_engine"];
    let version = engine["version"].as_u64().map(|v| format!("?version={v}")).unwrap_or_default();
    match engine["type"].as_str().unwrap_or_default() {
        "retell-llm" => {}
        "conversation-flow" => {
            let live = Live::NotComparable("multi-node (Retell conversation flow)".into());
            let tools = match str_of(engine, "conversation_flow_id") {
                None => ToolList::Error("Retell agent has no conversation_flow_id".into()),
                Some(flow_id) => {
                    let path = format!("/get-conversation-flow/{}{version}", segment(flow_id));
                    match get(http, &RETELL, &path, "Authorization", "Bearer ") {
                        Ok(flow) => ToolList::Live(retell_flow_tools(&flow)),
                        Err(e) => ToolList::Error(format!("can't read the conversation flow: {e}")),
                    }
                }
            };
            return Ok((live, tools));
        }
        "custom-llm" => {
            let why = "custom LLM: the prompt lives in your LLM server";
            let tools = ToolList::NotChecked("custom LLM: the tools live in your LLM server".into());
            return Ok((Live::NotComparable(why.into()), tools));
        }
        other => return Err(format!("Retell agent has an unknown response engine `{other}`")),
    }
    let Some(llm_id) = engine["llm_id"].as_str() else {
        return Err("Retell agent has no llm_id".into());
    };
    let llm = get(http, &RETELL, &format!("/get-retell-llm/{}{version}", segment(llm_id)), "Authorization", "Bearer ")?;
    Ok((retell_prompt(&llm), ToolList::Live(retell_llm_tools(&llm))))
}

// --- HTTP ------------------------------------------------------------------------------------

fn client() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("sopc/", env!("CARGO_PKG_VERSION")))
        .build();
    config.into()
}

/// GETs `path` from a platform and parses the JSON body. Errors are one plain line; the key is
/// never part of them.
fn get(http: &ureq::Agent, api: &Api, path: &str, header: &str, prefix: &str) -> Result<Json, String> {
    let key = std::env::var(api.key_var).unwrap_or_default();
    if key.trim().is_empty() {
        return Err(format!("{} is not set (needed to read {} agents)", api.key_var, api.name));
    }
    let base = std::env::var(api.url_var).ok().filter(|u| !u.trim().is_empty());
    let base = base.as_deref().unwrap_or(api.default_url).trim_end_matches('/').to_string();
    let url = format!("{base}{path}");
    let mut resp = http
        .get(&url)
        .header(header, &format!("{prefix}{}", key.trim()))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("can't reach {} ({base}): {e}", api.name))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    match status {
        200..=299 => {}
        401 | 403 => return Err(format!("{} refused the key (HTTP {status}); check {}", api.name, api.key_var)),
        404 => return Err(format!("not found on {} (HTTP 404)", api.name)),
        _ => return Err(format!("{} returned HTTP {status}", api.name)),
    }
    serde_json::from_str(&body).map_err(|e| format!("{} returned a response sopc can't read: {e}", api.name))
}

/// A path segment with anything but unreserved characters percent-encoded.
fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// --- where each platform keeps the prompt ----------------------------------------------------------

/// `conversation_config.agent.prompt.prompt`, unless a workflow routes calls through other nodes.
pub fn elevenlabs_prompt(agent: &Json) -> Live {
    let nodes = agent["workflow"]["nodes"].as_object();
    // A workflow with only start and end nodes runs the main prompt; anything else is multi-node.
    let routed =
        nodes.into_iter().flatten().filter(|(_, n)| !matches!(n["type"].as_str(), Some("start" | "end"))).count();
    if routed > 0 {
        return Live::NotComparable(format!("multi-node (ElevenLabs workflow with {routed} node(s))"));
    }
    let prompt = agent["conversation_config"]["agent"]["prompt"]["prompt"].as_str();
    Live::Prompt(prompt.unwrap_or_default().to_string())
}

/// The content of the `system` message in `model.messages`.
pub fn vapi_prompt(assistant: &Json) -> Live {
    let messages = assistant["model"]["messages"].as_array();
    let system: Vec<&Json> = messages.into_iter().flatten().filter(|m| m["role"] == "system").collect();
    if system.len() > 1 {
        return Live::NotComparable(format!("{} system messages", system.len()));
    }
    let content = system.first().and_then(|m| m["content"].as_str());
    Live::Prompt(content.unwrap_or_default().to_string())
}

/// `general_prompt`, unless the LLM has states with their own prompts.
pub fn retell_prompt(llm: &Json) -> Live {
    let states = llm["states"].as_array();
    let with_prompts =
        states.into_iter().flatten().filter(|s| s["state_prompt"].as_str().is_some_and(|p| !p.trim().is_empty()));
    let n = with_prompts.count();
    if n > 0 {
        return Live::NotComparable(format!("multi-node (Retell LLM with {n} state prompt(s))"));
    }
    Live::Prompt(llm["general_prompt"].as_str().unwrap_or_default().to_string())
}

// --- where each platform keeps the tools -------------------------------------------------------

/// The tools of an ElevenLabs agent, from `conversation_config.agent.prompt` and any workflow
/// nodes: inline `tools` (by `name`), `built_in_tools` (system tools such as `end_call`, by their
/// `name` or key), and `tool_ids` / `additional_tool_ids` (workspace tools, looked up by id).
/// MCP servers (`mcp_server_ids`, `native_mcp_server_ids`) make the list incomplete.
pub fn elevenlabs_tools(agent: &Json) -> LiveTools {
    fn walk(v: &Json, live: &mut LiveTools, known: &mut BTreeSet<String>) {
        let Some(obj) = v.as_object() else {
            v.as_array().into_iter().flatten().for_each(|x| walk(x, live, known));
            return;
        };
        for (key, value) in obj {
            match key.as_str() {
                "tools" if value.is_array() => {
                    for tool in value.as_array().into_iter().flatten() {
                        if tool["type"] == "mcp" {
                            live.opaque = Some(MCP.into());
                        } else if let Some(name) = str_of(tool, "name") {
                            live.names.insert(name.to_string());
                            known.extend(["id", "tool_id"].iter().filter_map(|k| str_of(tool, k)).map(String::from));
                        } else if let Some(id) = str_of(tool, "tool_id").or(str_of(tool, "id")) {
                            live.ids.insert(id.to_string());
                        }
                    }
                }
                "tool_ids" | "additional_tool_ids" => {
                    let ids = value.as_array().into_iter().flatten().filter_map(|i| i.as_str());
                    live.ids.extend(ids.filter(|i| !i.trim().is_empty()).map(String::from));
                }
                "built_in_tools" => {
                    for (name, tool) in value.as_object().into_iter().flatten() {
                        if tool.is_object() {
                            live.names.insert(str_of(tool, "name").unwrap_or(name).to_string());
                        }
                    }
                }
                "mcp_server_ids" | "native_mcp_server_ids" if has_items(value) => live.opaque = Some(MCP.into()),
                _ => walk(value, live, known),
            }
        }
    }
    let (mut live, mut known) = (LiveTools::default(), BTreeSet::new());
    walk(&agent["conversation_config"]["agent"], &mut live, &mut known);
    walk(&agent["workflow"], &mut live, &mut known);
    live.ids.retain(|id| !known.contains(id));
    live
}

/// An ElevenLabs workspace tool (`GET /v1/convai/tools/{id}`): `tool_config.name`, or a system
/// tool's `tool_config.params.system_tool_type`.
pub fn elevenlabs_tool(tool: &Json) -> Tool {
    let config = &tool["tool_config"];
    if config["type"] == "mcp" {
        return Tool::Mcp;
    }
    let name = str_of(config, "name").or(str_of(&config["params"], "system_tool_type"));
    name.map_or(Tool::Unknown, |n| Tool::Named(n.to_string()))
}

/// The tools of a Vapi assistant: `model.tools` (inline), `model.toolIds` (saved tools, looked
/// up by id), legacy `model.functions`, and the `endCallFunctionEnabled` /
/// `dialKeypadFunctionEnabled` switches (`endCall`, `dtmf`).
pub fn vapi_tools(assistant: &Json) -> LiveTools {
    let mut live = LiveTools::default();
    let model = &assistant["model"];
    for tool in model["tools"].as_array().into_iter().flatten() {
        live.add(vapi_tool(tool));
    }
    for f in model["functions"].as_array().into_iter().flatten() {
        live.names.extend(str_of(f, "name").map(String::from));
    }
    let ids = model["toolIds"].as_array().into_iter().flatten().filter_map(|i| i.as_str());
    live.ids.extend(ids.filter(|i| !i.trim().is_empty()).map(String::from));
    for (switch, name) in [("endCallFunctionEnabled", "endCall"), ("dialKeypadFunctionEnabled", "dtmf")] {
        if assistant[switch] == true || model[switch] == true {
            live.names.insert(name.into());
        }
    }
    live
}

/// A Vapi tool's name as the model sees it: `function.name`, else `name`, else its `type` (a
/// built-in such as `transferCall` or `endCall` without a custom function).
pub fn vapi_tool(tool: &Json) -> Tool {
    if tool["type"] == "mcp" {
        return Tool::Mcp;
    }
    let name = str_of(&tool["function"], "name").or(str_of(tool, "name")).or(str_of(tool, "type"));
    name.map_or(Tool::Unknown, |n| Tool::Named(n.to_string()))
}

/// The tools of a Retell LLM: `general_tools[].name` and `states[].tools[].name`.
pub fn retell_llm_tools(llm: &Json) -> LiveTools {
    let mut live = LiveTools::default();
    let states = llm["states"].as_array().into_iter().flatten().map(|s| &s["tools"]);
    for tools in std::iter::once(&llm["general_tools"]).chain(states) {
        let names = tools.as_array().into_iter().flatten().filter_map(|t| str_of(t, "name"));
        live.names.extend(names.map(String::from));
    }
    live
}

/// The tools of a Retell conversation flow: every `tools[].name` in it (the flow's own,
/// its components', and its nodes'). A node using a shared tool (`tool_type: shared`) makes the
/// list incomplete.
pub fn retell_flow_tools(flow: &Json) -> LiveTools {
    fn walk(v: &Json, live: &mut LiveTools) {
        if let Some(items) = v.as_array() {
            items.iter().for_each(|x| walk(x, live));
            return;
        }
        let Some(obj) = v.as_object() else { return };
        if v["tool_type"] == "shared" {
            live.opaque = Some("the flow uses shared tools, which sopc doesn't look up".into());
        }
        for (key, value) in obj {
            if key == "tools" {
                let names = value.as_array().into_iter().flatten().filter_map(|t| str_of(t, "name"));
                live.names.extend(names.map(String::from));
            }
            walk(value, live);
        }
    }
    let mut live = LiveTools::default();
    walk(flow, &mut live);
    live
}
