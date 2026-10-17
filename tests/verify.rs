//! Tests of `sopc verify` against a mock of the platforms' APIs.

use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const KEYS: [(&str, &str); 3] =
    [("ELEVENLABS_API_KEY", "el-secret-123"), ("VAPI_API_KEY", "vapi-secret-456"), ("RETELL_API_KEY", "rt-secret-789")];

/// A request the mock received: "GET /path?query" and its auth header value.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serves `routes` (path with query → (status, JSON body)) on a local port until the test ends.
fn mock(routes: BTreeMap<String, (u16, Json)>) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut auth = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                    break;
                }
                let (name, value) = line.split_once(':').unwrap_or_default();
                if ["authorization", "xi-api-key"].contains(&name.to_ascii_lowercase().as_str()) {
                    auth = value.trim().to_string();
                }
            }
            let mut parts = request.split_whitespace();
            let (method, path) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
            log.lock().unwrap().push((format!("{method} {path}"), auth));
            let (status, body) = routes.get(path).cloned().unwrap_or((404, json!({"detail": "not found"})));
            let body = body.to_string();
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes()));
        }
    });
    (url, seen)
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `sopc verify` in `root` with every key set except `unset`, against `url` for every platform.
fn verify(root: &Path, url: &str, args: &[&str], unset: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sopc"));
    cmd.arg("verify").args(args).arg("--dir").arg(root);
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
        cmd.env_remove(var);
    }
    for (k, v) in KEYS {
        if unset.contains(&k) {
            cmd.env_remove(k);
        } else {
            cmd.env(k, v);
        }
    }
    for var in ["SOPC_ELEVENLABS_URL", "SOPC_VAPI_URL", "SOPC_RETELL_URL"] {
        cmd.env(var, url);
    }
    let out = cmd.output().unwrap();
    let out = Output {
        code: out.status.code().unwrap(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    };
    for (_, key) in KEYS {
        assert!(!out.stdout.contains(key) && !out.stderr.contains(key), "a key was printed");
    }
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

/// Agents added to the fixture's three LiveKit ones: (sopc id, platform line).
const AGENTS: [(&str, &str); 11] = [
    ("el-sync", "elevenlabs: el_sync"),
    ("el-flow", "elevenlabs: el_flow"),
    ("vapi-drift", "vapi: asst_drift"),
    ("vapi-gone", "vapi: asst_gone"),
    ("retell-sync", "retell: agent_sync"),
    ("retell-states", "retell: agent_states"),
    ("retell-flow", "retell: agent_flow"),
    ("retell-draft", "retell: agent_draft"),
    ("el-typo", "elevenlabs: el_typo"),
    ("vapi-mcp", "vapi: asst_mcp"),
    ("vapi-badtool", "vapi: asst_badtool"),
];

/// The tools the fixture's agents name (the SOPs' tools, and get_sop for tool-delivered SOPs).
const TOOLS: [&str; 7] = [
    "check_capacity",
    "check_delivery_zone",
    "check_reservations",
    "get_sop",
    "lookup_allergens",
    "place_order",
    "transfer_to_staff",
];

/// Tool definitions named `names`, as `{"type", "name"}`.
fn named(names: &[&str]) -> Json {
    names.iter().map(|n| json!({"type": "custom", "name": n})).collect()
}

/// Vapi function tools named `names`.
fn functions(names: &[&str]) -> Json {
    names.iter().map(|n| json!({"type": "function", "function": {"name": n}})).collect()
}

/// The fixture with an agent per case, and each agent's compiled prompt.
fn workspace() -> (tempfile::TempDir, PathBuf, BTreeMap<String, String>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sops");
    copy_dir(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/restaurants/sops"), &root);
    let template = std::fs::read_to_string(root.join("agents/luigis-trattoria.yaml")).unwrap();
    for (id, platform) in AGENTS {
        let text = template.replace("livekit: luigis-trattoria", platform);
        std::fs::write(root.join(format!("agents/{id}.yaml")), text).unwrap();
    }
    let out = dir.path().join("out");
    let status = Command::new(env!("CARGO_BIN_EXE_sopc")).arg("--dir").arg(&root).arg("-o").arg(&out).output();
    assert!(status.unwrap().status.success());
    let prompts = AGENTS
        .iter()
        .map(|(id, _)| (id.to_string(), std::fs::read_to_string(out.join(format!("{id}.prompt.md"))).unwrap()))
        .collect();
    (dir, root, prompts)
}

/// Each platform's API, answering for the agents in AGENTS.
fn routes(prompts: &BTreeMap<String, String>) -> BTreeMap<String, (u16, Json)> {
    // `tools` holds the prompt's tool fields: tools, tool_ids and built_in_tools.
    let el = |prompt: &str, tools: Json, workflow: Json| {
        let mut config = tools;
        config["prompt"] = json!(prompt);
        json!({"agent_id": "x", "conversation_config": {"agent": {"prompt": config}}, "workflow": workflow})
    };
    // Windows line endings and trailing spaces don't count as drift.
    let crlf = prompts["el-sync"].replace('\n', "\r\n") + "  \r\n";
    let drifted = prompts["vapi-drift"].replace("Takeout and reservations", "Takeout only tonight");
    let flow = json!({
        "nodes": {"start_node": {"type": "start"}, "billing": {"type": "override_agent", "additional_tool_ids": ["tool_cap"]}, "end_node": {"type": "end"}},
        "edges": {}
    });
    let retell_agent = |engine: Json| json!({"agent_id": "x", "response_engine": engine});
    let versions = |items: Json| json!({"items": items, "has_more": false});
    let llm = |prompt: &str| json!({"general_prompt": prompt, "states": [], "general_tools": named(&TOOLS)});
    let vapi = |prompt: &str, model: Json| {
        let mut model = model;
        model["messages"] = json!([{"role": "system", "content": prompt}]);
        json!({"id": "x", "model": model})
    };
    let entries = [
        // Inline tools, workspace tools by id, and an unset built-in tool.
        (
            "/v1/convai/agents/el_sync",
            200,
            el(
                &crlf,
                json!({"tools": named(&TOOLS[..5]), "tool_ids": ["tool_place", "tool_staff"], "built_in_tools": {"end_call": null}}),
                json!({"nodes": {}, "edges": {}}),
            ),
        ),
        (
            "/v1/convai/tools/tool_place",
            200,
            json!({"id": "tool_place", "tool_config": {"type": "webhook", "name": "place_order"}}),
        ),
        (
            "/v1/convai/tools/tool_staff",
            200,
            json!({"id": "tool_staff", "tool_config": {"type": "client", "name": "transfer_to_staff"}}),
        ),
        (
            "/v1/convai/tools/tool_cap",
            200,
            json!({"id": "tool_cap", "tool_config": {"type": "webhook", "name": "check_capacity"}}),
        ),
        // check_capacity comes from a workflow node.
        ("/v1/convai/agents/el_flow", 200, el("anything", json!({"tools": named(&TOOLS[1..])}), flow)),
        // A typo, a case difference, a missing tool and a system tool no SOP names.
        (
            "/v1/convai/agents/el_typo",
            200,
            el(
                &prompts["el-typo"],
                json!({
                    "tools": named(&["check_delivery_zone", "check_reservations", "get_sop", "lookup_alergens", "Place_Order", "transfer_to_staff"]),
                    "built_in_tools": {"end_call": {"name": "end_call", "type": "system"}, "transfer_to_number": null}
                }),
                json!({}),
            ),
        ),
        (
            "/assistant/asst_drift",
            200,
            vapi(&drifted, json!({"tools": functions(&TOOLS[..6]), "toolIds": ["vt_staff"]})),
        ),
        (
            "/tool/vt_staff",
            200,
            json!({"id": "vt_staff", "type": "function", "function": {"name": "transfer_to_staff"}}),
        ),
        // An MCP server may provide the tools that aren't listed.
        (
            "/assistant/asst_mcp",
            200,
            vapi(
                &prompts["vapi-mcp"],
                json!({"tools": [{"type": "mcp", "server": {"url": "https://x"}}, {"type": "transferCall"}], "toolIds": []}),
            ),
        ),
        ("/assistant/asst_badtool", 200, vapi(&prompts["vapi-badtool"], json!({"toolIds": ["vt_gone"]}))),
        // Version 4 is a draft with an edited prompt; version 3 is published and serves calls.
        (
            "/list-agent-versions/agent_sync?limit=1000&sort_order=descending",
            200,
            versions(json!([
                {"version": 4, "is_published": false}, {"version": 3, "is_published": true},
                {"version": 2, "is_published": true}
            ])),
        ),
        ("/get-agent/agent_sync", 200, retell_agent(json!({"type": "retell-llm", "llm_id": "llm_sync", "version": 4}))),
        (
            "/get-agent/agent_sync?version=4",
            200,
            retell_agent(json!({"type": "retell-llm", "llm_id": "llm_sync", "version": 4})),
        ),
        ("/get-retell-llm/llm_sync?version=4", 200, llm("A draft nobody has published yet.")),
        (
            "/get-agent/agent_sync?version=3",
            200,
            retell_agent(json!({"type": "retell-llm", "llm_id": "llm_sync", "version": 3})),
        ),
        ("/get-retell-llm/llm_sync?version=3", 200, llm(&prompts["retell-sync"])),
        // The published version is on the second page.
        (
            "/list-agent-versions/agent_states?limit=1000&sort_order=descending",
            200,
            json!({"items": [{"version": 9, "is_published": false}], "has_more": true, "pagination_key": "p 2"}),
        ),
        (
            "/list-agent-versions/agent_states?limit=1000&sort_order=descending&pagination_key=p%202",
            200,
            versions(json!([{"version": 1, "is_published": true}])),
        ),
        ("/get-agent/agent_states?version=1", 200, retell_agent(json!({"type": "retell-llm", "llm_id": "llm_states"}))),
        // Tools split between the LLM and a state.
        (
            "/get-retell-llm/llm_states",
            200,
            json!({"general_prompt": "x", "general_tools": named(&TOOLS[..3]),
                   "states": [{"name": "a", "state_prompt": "Greet.", "tools": named(&TOOLS[3..])}]}),
        ),
        (
            "/list-agent-versions/agent_flow?limit=1000&sort_order=descending",
            200,
            versions(json!([{"version": 0, "is_published": true}])),
        ),
        (
            "/get-agent/agent_flow?version=0",
            200,
            retell_agent(json!({"type": "conversation-flow", "conversation_flow_id": "flow_1", "version": 2})),
        ),
        // Tools of the flow, a component, and a node.
        (
            "/get-conversation-flow/flow_1?version=2",
            200,
            json!({
                "conversation_flow_id": "flow_1", "tools": named(&TOOLS[..4]),
                "components": [{"name": "c", "tools": named(&TOOLS[4..6]), "nodes": []}],
                "nodes": [{"id": "n", "type": "subagent", "tools": named(&TOOLS[6..])}, {"id": "f", "type": "function", "tool_id": "t1", "tool_type": "local"}]
            }),
        ),
        // Never published: the latest draft is compared, with a note.
        (
            "/list-agent-versions/agent_draft?limit=1000&sort_order=descending",
            200,
            versions(json!([{"version": 1, "is_published": false}, {"version": 0, "is_published": false}])),
        ),
        (
            "/get-agent/agent_draft",
            200,
            retell_agent(json!({"type": "retell-llm", "llm_id": "llm_draft", "version": 1})),
        ),
        ("/get-retell-llm/llm_draft?version=1", 200, llm(&prompts["retell-draft"])),
    ];
    entries.into_iter().map(|(path, status, body)| (path.to_string(), (status, body))).collect()
}

fn line<'a>(out: &'a Output, id: &str) -> &'a str {
    let prefix = format!("{id} ");
    out.stdout.lines().find(|l| l.starts_with(&prefix)).unwrap_or_else(|| panic!("no {id} in {}", out.stdout))
}

#[test]
fn verify_reports_every_agent() {
    let (_dir, root, prompts) = workspace();
    let (url, seen) = mock(routes(&prompts));
    let out = verify(&root, &url, &[], &[]);
    assert_eq!(out.code, 1, "{}\n{}", out.stdout, out.stderr);
    assert!(line(&out, "el-sync").ends_with("in sync"), "{}", out.stdout);
    assert!(line(&out, "retell-sync").ends_with("in sync"), "{}", out.stdout);
    assert!(line(&out, "vapi-drift").ends_with("drifted"), "{}", out.stdout);
    assert!(line(&out, "vapi-gone").contains("error: not found on Vapi (HTTP 404)"), "{}", out.stdout);
    assert!(line(&out, "el-flow").contains("not comparable: multi-node (ElevenLabs workflow with 1 node(s))"));
    assert!(line(&out, "retell-states").contains("not comparable: multi-node (Retell LLM with 1 state prompt(s))"));
    assert!(line(&out, "retell-flow").contains("not comparable: multi-node (Retell conversation flow)"));
    assert!(line(&out, "retell-draft").ends_with("in sync: not published on Retell; compared the latest draft"));
    assert!(line(&out, "tonys-pizza").contains("skipped: LiveKit"), "{}", out.stdout);
    // The drift is shown as a diff from the compiled prompt to the live one.
    assert!(out.stdout.contains("--- compiled/vapi-drift.prompt.md\n+++ live/vapi:asst_drift\n"), "{}", out.stdout);
    assert!(out.stdout.contains("\n+Luigi's is a sit-down Italian restaurant in Queens. Takeout only tonight"));
    assert!(
        out.stdout.ends_with(
            "\n6 in sync, 1 drifted, 3 not comparable, 3 skipped, 1 error, 1 with missing tools, 1 tool error\n"
        ),
        "{}",
        out.stdout
    );
    // Only GETs, with each platform's auth header.
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|(req, _)| req.starts_with("GET ")), "{seen:?}");
    let auth = |path: &str| seen.iter().find(|(r, _)| r == &format!("GET {path}")).map(|(_, a)| a.clone());
    assert_eq!(auth("/v1/convai/agents/el_sync").as_deref(), Some("el-secret-123"));
    assert_eq!(auth("/assistant/asst_drift").as_deref(), Some("Bearer vapi-secret-456"));
    assert_eq!(auth("/get-retell-llm/llm_sync?version=3").as_deref(), Some("Bearer rt-secret-789"));
}

#[test]
fn verify_in_sync_exits_0() {
    let (_dir, root, prompts) = workspace();
    let (url, _) = mock(routes(&prompts));
    let out = verify(&root, &url, &["el-sync", "agent_sync", "el-flow", "tonys-pizza"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    assert_eq!(out.stdout.lines().count(), 6, "{}", out.stdout); // 4 agents, a blank line, the summary
    assert!(out.stdout.ends_with("\n2 in sync, 1 not comparable, 1 skipped\n"), "{}", out.stdout);
}

#[test]
fn verify_filters_by_id_and_rejects_unknown_ones() {
    let (_dir, root, prompts) = workspace();
    let (url, seen) = mock(routes(&prompts));
    let out = verify(&root, &url, &["vapi:asst_drift"], &[]);
    assert_eq!(out.code, 1);
    assert!(out.stdout.ends_with("\n1 drifted\n"), "{}", out.stdout);
    // The assistant, and the saved tool it lists by id.
    assert_eq!(seen.lock().unwrap().len(), 2);
    let out = verify(&root, &url, &["nope"], &[]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("unknown agent(s): nope"), "{}", out.stderr);
}

#[test]
fn verify_names_a_missing_key() {
    let (_dir, root, prompts) = workspace();
    let (url, seen) = mock(routes(&prompts));
    let out = verify(&root, &url, &["el-sync", "retell-sync"], &["RETELL_API_KEY"]);
    assert_eq!(out.code, 1);
    assert!(line(&out, "el-sync").ends_with("in sync"));
    assert!(line(&out, "retell-sync").ends_with("error: RETELL_API_KEY is not set (needed to read Retell agents)"));
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|(r, _)| r.starts_with("GET /v1/convai/")), "no Retell request without a key: {seen:?}");
}

#[test]
fn verify_json_report() {
    let (_dir, root, prompts) = workspace();
    let (url, _) = mock(routes(&prompts));
    let out = verify(&root, &url, &["--json", "el-sync", "vapi-drift", "vapi-gone", "luigis-trattoria"], &[]);
    assert_eq!(out.code, 1);
    let report: Json = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(
        report["summary"],
        json!({"in_sync": 1, "drifted": 1, "not_comparable": 0, "skipped": 1, "error": 1, "missing_tools": 0, "tool_errors": 0})
    );
    let agents = report["agents"].as_array().unwrap();
    let ids: Vec<&str> = agents.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["el-sync", "vapi-drift", "vapi-gone", "luigis-trattoria"]);
    let referenced: Vec<Json> =
        TOOLS.iter().map(|t| json!({"name": t, "required": (["get_sop", "lookup_allergens"].contains(t))})).collect();
    assert_eq!(
        agents[0],
        json!({"id": "el-sync", "platform": "elevenlabs", "platform_id": "el_sync", "platform_ref": "elevenlabs:el_sync",
               "status": "in_sync", "reason": null, "diff": null,
               "tools": {"checked": true, "referenced": referenced, "missing": [], "unreferenced": [], "unconfirmed": [],
                         "note": null, "error": null}})
    );
    assert_eq!(agents[1]["status"], "drifted");
    assert_eq!(agents[1]["tools"]["unreferenced"], json!([]));
    assert!(agents[1]["diff"].as_str().unwrap().starts_with("--- compiled/vapi-drift.prompt.md\n+++ live/"));
    assert_eq!(agents[2]["status"], "error");
    assert_eq!(agents[2]["tools"], Json::Null);
    assert_eq!(agents[3]["status"], "skipped");
    assert_eq!(agents[3]["tools"], Json::Null);
}

#[test]
fn verify_reports_an_unreachable_platform() {
    let (_dir, root, _) = workspace();
    // Nothing listens on a port that was just freed.
    let url = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let out = verify(&root, &url, &["vapi-drift"], &[]);
    assert_eq!(out.code, 1);
    assert!(line(&out, "vapi-drift").contains("error: can't reach Vapi"), "{}", out.stdout);
}

#[test]
fn verify_reads_the_published_retell_version() {
    let (_dir, root, prompts) = workspace();
    let (url, seen) = mock(routes(&prompts));
    let out = verify(&root, &url, &["retell-sync", "retell-states"], &[]);
    assert!(line(&out, "retell-sync").ends_with("in sync"), "{}", out.stdout);
    assert!(line(&out, "retell-states").contains("not comparable"), "{}", out.stdout);
    let seen: Vec<String> = seen.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
    // The draft (version 4) and the unversioned agent are never read.
    for path in ["/get-agent/agent_sync", "/get-agent/agent_sync?version=4", "/get-retell-llm/llm_sync?version=4"] {
        assert!(!seen.contains(&format!("GET {path}")), "{path} was read: {seen:?}");
    }
    assert!(seen.contains(&"GET /get-retell-llm/llm_sync?version=3".to_string()), "{seen:?}");
    assert!(seen.contains(&"GET /get-agent/agent_states?version=1".to_string()), "{seen:?}");
}

#[test]
fn verify_compares_an_unpublished_retell_draft_with_a_note() {
    let (_dir, root, mut prompts) = workspace();
    prompts.insert("retell-draft".into(), "Edited in the dashboard.".into());
    let (url, _) = mock(routes(&prompts));
    let out = verify(&root, &url, &["--json", "retell-draft"], &[]);
    assert_eq!(out.code, 1);
    let report: Json = serde_json::from_str(&out.stdout).unwrap();
    let agent = &report["agents"][0];
    assert_eq!(agent["status"], "drifted");
    assert_eq!(agent["reason"], "not published on Retell; compared the latest draft");
}

#[test]
fn verify_finds_tools_missing_on_the_platform() {
    let (_dir, root, prompts) = workspace();
    let (url, _) = mock(routes(&prompts));
    // The prompt is in sync, but the agent lacks tools its SOPs name.
    let out = verify(&root, &url, &["el-typo"], &[]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(line(&out, "el-typo").ends_with("in sync"), "{}", out.stdout);
    for expected in [
        "  tools: missing check_capacity\n",
        "  tools: missing lookup_allergens (required step); did you mean lookup_alergens?\n",
        "  tools: missing place_order; did you mean Place_Order?\n",
        "  tools: not in any SOP: Place_Order, end_call, lookup_alergens\n",
    ] {
        assert!(out.stdout.contains(expected), "{expected:?} not in\n{}", out.stdout);
    }
    assert!(out.stdout.ends_with("\n1 in sync, 1 with missing tools\n"), "{}", out.stdout);

    let out = verify(&root, &url, &["--json", "el-typo"], &[]);
    let report: Json = serde_json::from_str(&out.stdout).unwrap();
    let tools = &report["agents"][0]["tools"];
    assert_eq!(
        tools["missing"],
        json!([
            {"name": "check_capacity", "required": false, "suggestion": null},
            {"name": "lookup_allergens", "required": true, "suggestion": "lookup_alergens"},
            {"name": "place_order", "required": false, "suggestion": "Place_Order"},
        ])
    );
    assert_eq!(tools["unreferenced"], json!(["Place_Order", "end_call", "lookup_alergens"]));
    assert_eq!(report["summary"]["missing_tools"], 1);
    assert_eq!(report["ok"], false);
}

#[test]
fn verify_reads_tools_from_every_platform_shape() {
    let (_dir, root, prompts) = workspace();
    let (url, seen) = mock(routes(&prompts));
    // ElevenLabs workflow node tool ids, Vapi tool ids, Retell state tools and conversation flow tools.
    let out = verify(&root, &url, &["el-flow", "vapi-drift", "retell-states", "retell-flow", "retell-sync"], &[]);
    assert!(!out.stdout.contains("tools: missing"), "{}", out.stdout);
    assert!(!out.stdout.contains("tools: error"), "{}", out.stdout);
    let seen: Vec<String> = seen.lock().unwrap().iter().map(|(r, _)| r.clone()).collect();
    for path in ["/v1/convai/tools/tool_cap", "/tool/vt_staff", "/get-conversation-flow/flow_1?version=2"] {
        assert!(seen.contains(&format!("GET {path}")), "{path} not read: {seen:?}");
    }
}

#[test]
fn verify_doesnt_fail_on_tools_an_mcp_server_may_provide() {
    let (_dir, root, prompts) = workspace();
    let (url, _) = mock(routes(&prompts));
    let out = verify(&root, &url, &["vapi-mcp"], &[]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("  tools: not confirmed: check_capacity, "), "{}", out.stdout);
    assert!(out.stdout.contains("(the agent uses MCP servers, whose tools sopc can't list)\n"), "{}", out.stdout);
    assert!(out.stdout.contains("  tools: not in any SOP: transferCall\n"), "{}", out.stdout);
}

#[test]
fn verify_fails_when_a_tool_cant_be_read() {
    let (_dir, root, prompts) = workspace();
    let (url, _) = mock(routes(&prompts));
    let out = verify(&root, &url, &["vapi-badtool"], &[]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(out.stdout.contains("  tools: error: can't read tool vt_gone: not found on Vapi (HTTP 404)\n"));
    assert!(out.stdout.ends_with("\n1 in sync, 1 tool error\n"), "{}", out.stdout);
}
