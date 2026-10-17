//! The sopc file formats (instructions, SOPs, agents, sopc.yaml), parsed from YAML, and their
//! canonical JSON, which lock.json block hashes are taken over.
//!
//! The field lists below fix both what a file may contain and the order of keys in the
//! canonical JSON. They must stay in sync with spec/*.schema.json (a test checks this);
//! changing their order changes every hash in every lock.json.

use crate::text::sha256_hex;
use serde_json::{json, Value as Json};
use serde_yaml_ng::{Mapping, Value};

pub const PLATFORMS: [&str; 4] = ["livekit", "vapi", "elevenlabs", "retell"];
pub const DELIVERIES: [&str; 3] = ["prompt", "auto", "tool"];

pub const INSTRUCTION_FIELDS: &[&str] = &["id", "text"];
pub const SOP_FIELDS: &[&str] = &[
    "id",
    "name",
    "delivery",
    "description",
    "scope",
    "guidance",
    "procedureSteps",
    "forbiddenActions",
    "warningSigns",
];
pub const AGENT_FIELDS: &[&str] = &["id", "livekit", "vapi", "elevenlabs", "retell", "context", "blocks", "variables"];
pub const CONFIG_FIELDS: &[&str] = &["version", "variables", "groups", "sops_heading"];
pub const STEP_FIELDS: &[&str] = &["text", "tool", "required"];
/// Fields a file must set ("id" and an instruction's "text" are filled in by the loader).
pub const SOP_REQUIRED: &[&str] = &["name", "procedureSteps"];
pub const STEP_REQUIRED: &[&str] = &["text"];

/// Fields of the format before `sopc migrate` (v0.0.8 and earlier), by file kind. Finding one
/// means the folder needs migrating.
pub const OLD_INSTRUCTION_FIELDS: &[&str] = &["agents", "exclude", "inherits", "position"];
pub const OLD_SOP_FIELDS: &[&str] = &["agents", "exclude"];
pub const OLD_AGENT_FIELDS: &[&str] = &["inherits", "exclude", "instructions"];
pub const OLD_CONFIG_FIELDS: &[&str] = &["sop_order"];
/// Fields of instructions and SOPs that sopc v0.0.9 and earlier read and the format no longer
/// has. `sopc migrate` removes them; nothing else reads them.
pub const REMOVED_FIELDS: &[&str] = &["locked"];

/// A step, forbidden action or warning sign.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Step {
    pub plain: bool, // written as a plain string
    pub text: String,
    pub tool: Option<String>,
    pub required: bool,
}

impl Step {
    /// The tool name, if one is set and not empty.
    pub fn tool(&self) -> Option<&str> {
        self.tool.as_deref().filter(|t| !t.is_empty())
    }
}

/// One rule of a procedure: a step, a "never" (forbidden action) or a warning sign, with an id
/// that QA tools can attribute a violation to.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: String,
    /// "step", "never" or "warning".
    pub kind: &'static str,
    /// 1-based position among the rules of its kind.
    pub index: usize,
    /// The source text (placeholders not filled), trimmed, whitespace collapsed.
    pub text: String,
}

impl Rule {
    pub fn to_json(&self) -> Json {
        json!({"id": self.id, "kind": self.kind, "index": self.index, "text": self.text})
    }
}

/// Text trimmed, with each run of whitespace inside it collapsed to one space.
pub fn normalize_rule_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A rule's id: the first 12 hex characters of sha256("<procedure>\x1f<kind>\x1f<normalized text>").
/// It changes when the text does (an edited rule is a new rule), not when rules are reordered or
/// only whitespace changes, and it is the same for every agent (placeholders aren't filled).
pub fn rule_id(procedure: &str, kind: &str, text: &str) -> String {
    let mut hash = sha256_hex(&format!("{procedure}\x1f{kind}\x1f{}", normalize_rule_text(text)));
    hash.truncate(12);
    hash
}

/// Shared prompt text that isn't a procedure: instructions/<id>.md.
#[derive(Clone, Debug, Default)]
pub struct Instruction {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Sop {
    pub id: String,
    /// The file it was read from, e.g. "procedures/allergen-check.md" (not part of its JSON).
    pub file: String,
    pub name: String,
    pub delivery: String,
    pub description: String,
    pub scope: String,
    pub guidance: String,
    pub procedure_steps: Vec<Step>,
    pub forbidden_actions: Vec<Step>,
    pub warning_signs: Vec<Step>,
}

impl Sop {
    /// Its steps, then its nevers, then its warning signs, each with its id.
    pub fn rules(&self) -> Vec<Rule> {
        let kinds =
            [("step", &self.procedure_steps), ("never", &self.forbidden_actions), ("warning", &self.warning_signs)];
        let mut out = vec![];
        for (kind, list) in kinds {
            out.extend(list.iter().enumerate().map(|(i, s)| Rule {
                id: rule_id(&self.id, kind, &s.text),
                kind,
                index: i + 1,
                text: normalize_rule_text(&s.text),
            }));
        }
        out
    }
}

/// An instruction or an SOP: what an agent's `blocks` list names (besides groups).
#[derive(Clone, Debug)]
pub enum Block {
    Instruction(Instruction),
    Sop(Sop),
}

impl Block {
    pub fn id(&self) -> &str {
        match self {
            Block::Instruction(i) => &i.id,
            Block::Sop(s) => &s.id,
        }
    }
    /// The kind recorded in lock.json: "instruction" or "sop".
    pub fn kind(&self) -> &'static str {
        match self {
            Block::Instruction(_) => "instruction",
            Block::Sop(_) => "sop",
        }
    }
    pub fn canonical_json(&self) -> String {
        match self {
            Block::Instruction(i) => i.canonical_json(),
            Block::Sop(s) => s.canonical_json(),
        }
    }
}

/// Placeholder values, in the order they were written.
#[derive(Clone, Debug, Default)]
pub struct Vars(pub Vec<(String, String)>);

impl Vars {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    pub fn set(&mut self, key: String, value: String) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key, value)),
        }
    }
    /// `self` overridden by `other`.
    pub fn merged(&self, other: &Vars) -> Vars {
        let mut out = self.clone();
        for (k, v) in &other.0 {
            out.set(k.clone(), v.clone());
        }
        out
    }
}

#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub id: String,
    pub platforms: [Option<String>; 4], // livekit, vapi, elevenlabs, retell
    /// Text only this agent gets; first in its prompt.
    pub context: String,
    /// Instruction, SOP and group ids, in prompt order.
    pub blocks: Vec<String>,
    pub variables: Vars,
}

impl Agent {
    /// The one platform the agent sets, e.g. "livekit".
    pub fn platform(&self) -> &'static str {
        let i = self.platforms.iter().position(|p| p.as_deref().is_some_and(|s| !s.is_empty()));
        i.map_or("", |i| PLATFORMS[i])
    }
    /// The platform's own id for the agent.
    pub fn platform_id(&self) -> &str {
        let i = PLATFORMS.iter().position(|p| *p == self.platform()).unwrap_or(0);
        self.platforms[i].as_deref().unwrap_or("")
    }
    /// e.g. "livekit:tonys-pizza".
    pub fn platform_ref(&self) -> String {
        format!("{}:{}", self.platform(), self.platform_id())
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub variables: Vars,
    /// Group name → block and group ids, in order.
    pub groups: Vec<(String, Vec<String>)>,
    pub sops_heading: String,
}

impl Config {
    pub fn group(&self, name: &str) -> Option<&[String]> {
        self.groups.iter().find(|(n, _)| n == name).map(|(_, ids)| ids.as_slice())
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { variables: Vars::default(), groups: vec![], sops_heading: "## Procedures".into() }
    }
}

// --- canonical JSON --------------------------------------------------------------------------

fn steps_json(steps: &[Step]) -> Json {
    let items = steps.iter().map(|s| {
        if s.plain {
            json!(s.text)
        } else {
            json!({"text": s.text, "tool": s.tool, "required": s.required})
        }
    });
    Json::Array(items.collect())
}

impl Instruction {
    pub fn canonical_json(&self) -> String {
        json!({"id": self.id, "text": self.text}).to_string()
    }
}

impl Sop {
    pub fn canonical_json(&self) -> String {
        json!({
            "id": self.id, "name": self.name, "delivery": self.delivery,
            "description": self.description, "scope": self.scope, "guidance": self.guidance,
            "procedureSteps": steps_json(&self.procedure_steps),
            "forbiddenActions": steps_json(&self.forbidden_actions),
            "warningSigns": steps_json(&self.warning_signs),
        })
        .to_string()
    }
}

impl Agent {
    pub fn canonical_json(&self) -> String {
        let vars: serde_json::Map<String, Json> = self.variables.0.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let [livekit, vapi, elevenlabs, retell] = &self.platforms;
        json!({
            "id": self.id, "livekit": livekit, "vapi": vapi, "elevenlabs": elevenlabs, "retell": retell,
            "context": self.context, "blocks": self.blocks, "variables": vars,
        })
        .to_string()
    }
}

// --- parsing -----------------------------------------------------------------------------------

/// Collects "<location>: <problem>" messages while reading one file.
#[derive(Default)]
pub struct Errors(pub Vec<String>);

impl Errors {
    fn add(&mut self, loc: &str, msg: &str) {
        self.0.push(if loc.is_empty() { msg.to_string() } else { format!("{loc}: {msg}") });
    }
}

/// Text of a scalar (for messages and keys).
pub fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        Value::Tagged(t) => scalar_text(&t.value),
        other => serde_yaml_ng::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

fn untag(v: &Value) -> &Value {
    match v {
        Value::Tagged(t) => untag(&t.value),
        v => v,
    }
}

fn join(loc: &str, key: &str) -> String {
    if loc.is_empty() {
        key.to_string()
    } else {
        format!("{loc}.{key}")
    }
}

/// Reads the declared fields of one mapping, reporting unknown and missing ones.
struct Fields<'a> {
    map: &'a Mapping,
    loc: String,
}

impl<'a> Fields<'a> {
    fn new(map: &'a Mapping, loc: &str, known: &[&str], required: &[&str], e: &mut Errors) -> Self {
        for name in required {
            if map.get(*name).is_none() {
                e.add(&join(loc, name), "required field missing");
            }
        }
        for key in map.keys() {
            match untag(key) {
                Value::String(k) if known.contains(&k.as_str()) => {}
                Value::String(k) => e.add(&join(loc, k), "unknown field"),
                k => e.add(&join(loc, &scalar_text(k)), "field names must be text"),
            }
        }
        Fields { map, loc: loc.to_string() }
    }

    fn get(&self, name: &str) -> Option<(&'a Value, String)> {
        self.map.get(name).map(|v| (untag(v), join(&self.loc, name)))
    }

    fn string(&self, name: &str, e: &mut Errors) -> Option<String> {
        self.get(name).map(|(v, loc)| to_string(v, &loc, e))
    }

    fn opt_string(&self, name: &str, e: &mut Errors) -> Option<String> {
        match self.get(name) {
            Some((Value::Null, _)) | None => None,
            Some((v, loc)) => Some(to_string(v, &loc, e)),
        }
    }

    fn list(&self, name: &str, e: &mut Errors) -> Vec<String> {
        self.get(name).map(|(v, loc)| to_list(v, &loc, e)).unwrap_or_default()
    }

    fn boolean(&self, name: &str, e: &mut Errors) -> bool {
        self.get(name).is_some_and(|(v, loc)| to_bool(v, &loc, e))
    }

    /// name → list of text, in order.
    fn lists(&self, name: &str, e: &mut Errors) -> Vec<(String, Vec<String>)> {
        let Some((v, loc)) = self.get(name) else { return vec![] };
        let Value::Mapping(m) = v else {
            e.add(&loc, "expected a mapping of names to lists");
            return vec![];
        };
        let mut out: Vec<(String, Vec<String>)> = vec![];
        for (k, v) in m {
            let item = join(&loc, &scalar_text(k));
            let key = to_string(untag(k), &item, e);
            let list = match untag(v) {
                Value::Null => vec![],
                v => to_list(v, &item, e),
            };
            out.push((key, list));
        }
        out
    }

    fn choice(&self, name: &str, choices: &[&str], e: &mut Errors) -> String {
        match self.get(name) {
            None => choices[0].to_string(),
            Some((Value::String(s), _)) if choices.contains(&s.as_str()) => s.clone(),
            Some((_, loc)) => {
                let quoted: Vec<String> = choices.iter().map(|c| format!("'{c}'")).collect();
                e.add(&loc, &format!("must be one of {}", quoted.join(", ")));
                choices[0].to_string()
            }
        }
    }

    fn vars(&self, name: &str, e: &mut Errors) -> Vars {
        let mut out = Vars::default();
        let Some((v, loc)) = self.get(name) else { return out };
        let Value::Mapping(m) = v else {
            e.add(&loc, "expected a mapping of names to text");
            return out;
        };
        for (k, v) in m {
            let item = join(&loc, &scalar_text(k));
            let key = to_string(untag(k), &item, e);
            out.set(key, to_string(untag(v), &item, e));
        }
        out
    }

    fn steps(&self, name: &str, e: &mut Errors) -> Vec<Step> {
        let Some((v, loc)) = self.get(name) else { return vec![] };
        let Value::Sequence(items) = v else {
            e.add(&loc, "expected a list");
            return vec![];
        };
        let mut out = vec![];
        for (i, item) in items.iter().enumerate() {
            let loc = format!("{loc}.{i}");
            match untag(item) {
                Value::String(s) => out.push(Step { plain: true, text: s.clone(), ..Step::default() }),
                Value::Mapping(m) => {
                    let f = Fields::new(m, &loc, STEP_FIELDS, STEP_REQUIRED, e);
                    out.push(Step {
                        plain: false,
                        text: f.string("text", e).unwrap_or_default(),
                        tool: f.opt_string("tool", e),
                        required: f.boolean("required", e),
                    });
                }
                _ => e.add(&loc, "expected text, or a mapping with text, tool and required"),
            }
        }
        out
    }
}

fn to_string(v: &Value, loc: &str, e: &mut Errors) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => {
            e.add(loc, "expected text");
            String::new()
        }
    }
}

fn to_list(v: &Value, loc: &str, e: &mut Errors) -> Vec<String> {
    match v {
        Value::Sequence(items) => {
            items.iter().enumerate().map(|(i, x)| to_string(untag(x), &format!("{loc}.{i}"), e)).collect()
        }
        _ => {
            e.add(loc, "expected a list");
            vec![]
        }
    }
}

/// true/false; yes/no, on/off and 1/0 are accepted too.
fn to_bool(v: &Value, loc: &str, e: &mut Errors) -> bool {
    let text = match v {
        Value::Bool(b) => return *b,
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.to_lowercase(),
        _ => String::new(),
    };
    match text.as_str() {
        "1" | "true" | "yes" | "on" | "y" | "t" => true,
        "0" | "false" | "no" | "off" | "n" | "f" => false,
        _ => {
            e.add(loc, "expected true or false");
            false
        }
    }
}

fn finish<T>(value: T, e: Errors) -> Result<T, Vec<String>> {
    if e.0.is_empty() {
        Ok(value)
    } else {
        Err(e.0)
    }
}

/// An instruction's front matter, with "id" and "text" already set.
pub fn parse_instruction(map: &Mapping) -> Result<Instruction, Vec<String>> {
    let mut e = Errors::default();
    let f = Fields::new(map, "", INSTRUCTION_FIELDS, &[], &mut e);
    let instruction = Instruction {
        id: f.string("id", &mut e).unwrap_or_default(),
        text: f.string("text", &mut e).unwrap_or_default(),
    };
    finish(instruction, e)
}

/// A procedure file, with "id" already set.
pub fn parse_sop(map: &Mapping) -> Result<Sop, Vec<String>> {
    let mut e = Errors::default();
    // A missing or empty procedureSteps is reported as missing_steps before this is called.
    let f = Fields::new(map, "", SOP_FIELDS, &SOP_REQUIRED[..1], &mut e);
    let text = |name: &str, e: &mut Errors| f.string(name, e).unwrap_or_default();
    let sop = Sop {
        id: text("id", &mut e),
        name: text("name", &mut e),
        delivery: f.choice("delivery", &DELIVERIES, &mut e),
        description: text("description", &mut e),
        scope: text("scope", &mut e),
        guidance: text("guidance", &mut e),
        procedure_steps: f.steps("procedureSteps", &mut e),
        forbidden_actions: f.steps("forbiddenActions", &mut e),
        warning_signs: f.steps("warningSigns", &mut e),
        file: String::new(),
    };
    finish(sop, e)
}

/// An agent file, with "id" already set.
pub fn parse_agent(map: &Mapping) -> Result<Agent, Vec<String>> {
    let mut e = Errors::default();
    let f = Fields::new(map, "", AGENT_FIELDS, &[], &mut e);
    let agent = Agent {
        id: f.string("id", &mut e).unwrap_or_default(),
        platforms: PLATFORMS.map(|p| f.opt_string(p, &mut e)),
        context: f.string("context", &mut e).unwrap_or_default(),
        blocks: f.list("blocks", &mut e),
        variables: f.vars("variables", &mut e),
    };
    if e.0.is_empty() && agent.platforms.iter().filter(|p| p.as_deref().is_some_and(|s| !s.is_empty())).count() != 1 {
        e.add("", &format!("agent '{}' must set exactly one of {}", agent.id, PLATFORMS.join(", ")));
    }
    finish(agent, e)
}

/// sopc.yaml.
pub fn parse_config(map: &Mapping) -> Result<Config, Vec<String>> {
    let mut e = Errors::default();
    let f = Fields::new(map, "", CONFIG_FIELDS, &[], &mut e);
    if let Some((v, loc)) = f.get("version") {
        if v.as_f64() != Some(1.0) {
            e.add(&loc, "must be 1");
        }
    }
    let config = Config {
        variables: f.vars("variables", &mut e),
        groups: f.lists("groups", &mut e),
        sops_heading: f.string("sops_heading", &mut e).unwrap_or_else(|| Config::default().sops_heading),
    };
    finish(config, e)
}
