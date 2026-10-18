# sopc command reference

Every command and flag. `sopc --help` (or `sopc <command> --help`) prints the same information with examples; [FORMAT.md](FORMAT.md) describes the files sopc reads.

- [Install and uninstall](#install-and-uninstall)
- [How every command works](#how-every-command-works)
- Commands: [`sopc`](#sopc) · [`validate`](#sopc-validate) · [`lint`](#sopc-lint) · [`fmt`](#sopc-fmt) · [`plan`](#sopc-plan) · [`affected`](#sopc-affected) · [`verify`](#sopc-verify) · [`agents`](#sopc-agents) · [`export`](#sopc-export) · [`convert`](#sopc-convert) · [`overlap`](#sopc-overlap) · [`compare`](#sopc-compare) · [`migrate`](#sopc-migrate) · [`skills`](#sopc-skills) · [`guide`](#sopc-guide)

## Install and uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/amanmibra/sopc/main/install.sh | sh
```

Installs one `sopc` binary into `~/.local/bin` (macOS, Linux and Windows; amd64 and arm64) and verifies its checksum.

| Variable | Effect |
|---|---|
| `SOPC_REF=v0.0.16` | Install a specific release (default: the latest) |
| `SOPC_INSTALL_DIR=DIR` | Install somewhere other than `~/.local/bin` |

From source, with Rust: `cargo install --git https://github.com/amanmibra/sopc`.

To uninstall, delete the binary: `rm ~/.local/bin/sopc`. If you installed the import skill, also delete `.claude/skills/sopc-import` and `.agents/skills/sopc-import` from your repo.

## How every command works

**The folder.** Every command works on the sopc folder, the one containing `sopc.yaml`: `./sops` if `sops/sopc.yaml` exists, otherwise the current folder if `sopc.yaml` is there. Use `--dir DIR` (short: `-C DIR`) for another folder. It works before or after the command: `sopc --dir path/to/sops validate` and `sopc validate --dir path/to/sops` are the same.

**Output.** Results go to stdout; errors, warnings and progress notes go to stderr, so `--json` and id lists can be piped safely. File paths in messages are relative to the folder you ran sopc from (`sops/procedures/x.md: error [...] line 11: ...`). Color is turned off when output is piped, when `NO_COLOR` is set, or when `TERM=dumb`.

**Exit codes.**

| Code | Meaning |
|---|---|
| `0` | Success. `sopc lint` exits 0 even when it finds something, because its findings are advisory (unless `--strict`). |
| `1` | Failure: invalid files, a build that's out of date (`--check`), lint findings with `--strict`, a failed `compare`, a live prompt that drifted or couldn't be read (`verify`), files left unformatted to keep comments, or a git or file error. |
| `2` | Usage error: an unknown command or flag, a typo (sopc suggests the closest command), or a compile-only flag used with a command. |

A closed pipe (`sopc guide | head`) exits 0.

**Comparing with git.** `plan` and `affected` compare the current files with a git ref and print `Comparing with <ref> (<short sha>)` to stderr. Without `--against`, the ref is your default branch: `origin/HEAD`, then `origin/main`, `main`, `origin/master`, `master`. Git problems are reported in plain words, e.g. ``unknown git ref `origin/release`; check the name, or fetch it (`git fetch origin release`)``.

**Nothing destructive without `--yes`.** Commands that rewrite your files (`fmt`, `convert`) never remove comments silently. A file that would lose comments is left as it is and listed with those comments, and the command exits 1. Rerun with `-y`/`--yes` to allow it. `migrate` rewrites the whole folder, so it only shows its plan until you pass `--yes`.

## sopc

Compiles every agent's prompt into `build/` inside the sopc folder: one `<agent>.prompt.md` per agent, a `<agent>.tool.json` for agents with tool-delivered SOPs, and `lock.json`.

`lock.json` lists, per agent: its `platform_ref`, the prompt's `hash`, the `tools` its SOPs name, and `blocks`: the agent file first, then each block in prompt order, as `{"kind", "id", "hash"}`. `kind` is `agent`, `instruction` or `sop`; a hash is the sha256 of the block's canonical JSON. Groups aren't listed: their blocks are.

An SOP's entry also lists its `rules`: each step, never and warning sign as `{"id", "kind", "index", "text"}`, so QA tools can attribute a violation to one rule. `kind` is `step`, `never` or `warning`; `index` is its 1-based position within that kind; `text` is the source text, trimmed with whitespace collapsed (placeholders not filled). `id` is the first 12 hex characters of sha256(`<sop id>` U+001F `<kind>` U+001F `<text>`): the same in every agent, unchanged by reordering or whitespace, new when the text is edited.

```sh
sopc                         # compile ./sops into sops/build
sopc --check                 # exit 1 if sops/build is out of date (for CI)
sopc -C path/to/sops -o out  # compile another folder into out/
```

| Flag | Effect |
|---|---|
| `-o`, `--out DIR` | Write the build somewhere else (default: `DIR/build`) |
| `--check` | Write nothing; exit 1 and show what changed if the build is out of date |
| `--force` | Write into an `--out` folder that has other files but no `lock.json`. Only the files sopc builds are overwritten; nothing is removed |

Builds are written safely: files go to a temporary folder and are renamed into place, and only files the previous build created (as listed in its `lock.json`) are ever removed. Other files in the output folder are left alone. Without `--force`, sopc refuses to write into a non-empty folder it didn't build.

`-o` and `--check` apply only to compiling; `sopc --check validate` is a usage error.

## sopc validate

Finds errors in the files and prints every problem with its file, line (for Markdown SOPs) and a fix. Exits 1 if there are errors; warnings alone exit 0.

```sh
sopc validate
sopc validate --json
```

| Flag | Effect |
|---|---|
| `--json` | Prints `{"valid": bool, "issues": [{"code", "message", "path", "severity"}]}` to stdout instead, including files that fail to parse. Paths are relative to the sopc folder. |

The error codes are listed in [FORMAT.md](FORMAT.md#validation).

## sopc lint

Finds duplicated text and conflicting instructions in each agent's compiled prompt: the same sentence twice, the same sentence with different numbers (`10pm` vs `11pm`), "always X" vs "never X", near-identical sentences that have drifted, and variables an agent sets but never uses. The findings are advisory: lint exits 0, unless you pass `--strict`.

```sh
sopc lint
sopc lint --json
sopc lint --strict   # exit 1 if there are any findings (for CI)
```

| Flag | Effect |
|---|---|
| `--json` | Machine-readable output |
| `--strict` | Exit 1 if there are any findings, of any kind |

Each finding has a code: `numeric_conflict` (same sentence, different counts or limits), `negation_conflict` ("always X" vs "never X"), `duplicate_text`, `near_duplicate` and `unused_variable`.

Counts and limits are what [`overlap`](#sopc-overlap) calls rule words: `once`, `twice` and `one` to `ten`, and whole numbers used as a count, limit or duration (`up to 3`, `2 times`, `within 5 minutes`). Times, prices, phone numbers, addresses and ids are values, not counts: "Pickup only after 10pm." and "Pickup only after 11pm." are not a `numeric_conflict`.

**`--strict` fails on every kind, not only the conflicts.** Each one is cheap to fix and worth fixing before a merge: a duplicate is text to delete, an unused variable is a value to remove, and a near-duplicate is often a contradiction lint can't classify, because the word that changed isn't a number or a negation ("before" vs "after"). Lint has no way to silence a single finding, so when two sentences really should both stay, reword one.

**Lint compares wording, not meaning.** It only catches a contradiction when both sentences are written almost the same way. With a shared instruction that says "Never upsell more than once per call.", an agent's `context` saying "Never upsell more than twice per call." (`numeric_conflict`) or "Always upsell more than once per call." (`negation_conflict`) is caught, but the paraphrase "Upsell up to 2 times per call." is not. Read the prompt diffs from `sopc plan` for those.

## sopc fmt

Rewrites Markdown SOP files in one canonical style: sections in the standard order (Steps, Never, Warning signs), numbered steps, `-` bullets, one blank line between blocks, and tool markers as `` `tool: name` `` then `` `required` ``. Front matter is kept as written. It never changes a compiled prompt.

```sh
sopc fmt
sopc fmt --check   # for CI
sopc fmt --yaml    # YAML SOPs too
```

| Flag | Effect |
|---|---|
| `--check` | Write nothing; list files that would change and exit 1 if there are any |
| `--yaml` | Also rewrite YAML SOP files: keys in a fixed order, default settings left out, multi-line text as `\|` blocks. YAML comments below the top of the file can't be kept, so those files need `--yes` |
| `-y`, `--yes` | Allow rewrites that remove comments |

A file that would lose comments is left as it is and listed:

```
sops/procedures/takeout.yaml: not formatted: it would remove 2 comment(s); edit it by hand, or rerun with --yes to remove them
  line 2: # the heading agents see
  line 6: # ask first
```

## sopc plan

Shows which agents a change affects and because of which block, with a diff of each affected prompt. Compares with your default branch unless you pass `--against`.

```sh
sopc plan
sopc plan --summary
sopc plan --against v0.3.0
```

```console
$ sopc plan --summary
Comparing with origin/main (3f9a2c1)
3 agents change:
  instruction `brand-voice` edited → 3 agents: luigis-trattoria, sakura-sushi, tonys-pizza
```

Each line names a block and what happened to it: `edited` (its text or settings changed), `added` or `removed` (an agent started or stopped using it, through its `blocks` or a group). An agent file that changed is listed as `agent file`; a change only `sopc.yaml` explains (a default variable, a group's order, the SOP heading) as `` `sopc.yaml` edited ``.

| Flag | Effect |
|---|---|
| `--against REF` | Compare with this git branch, tag or commit |
| `--summary` | Leave out the diffs |
| `--json` | Machine-readable output |

If no default branch can be found, `plan` stops and asks for `--against`. A ref from before `sopc migrate` is built as migrating it would, so the plan right after migrating shows only each agent's own text moving to the top.

## sopc affected

Lists the agents to test for a change: those whose compiled prompt differs from the default branch (or `--against`). Made for CI: run your tests for just these agents. See the [behavior gate example](examples/behavior-gate).

```sh
sopc affected
sopc affected --format platform-ids
sopc affected --ci --all-if-none
```

| Flag | Effect |
|---|---|
| `--against REF` | Compare with this git branch, tag or commit |
| `--agents "IDS"` | Select these agents instead (sopc ids or platform ids, separated by spaces or commas). `--against` is ignored |
| `--all-if-none` | Select every agent when nothing else is selected |
| `--format ids\|platform-ids\|json` | `ids`: sopc ids, one per line (default). `platform-ids`: each agent's own id on its platform. `json`: everything, including which SOPs changed for each agent |
| `--ci` | Also write GitHub Actions outputs (`ids`, `platform_ids`, `matrix`, `count`, `all`) and a step summary |

If no default branch can be found, `affected` selects every agent and prints a warning, unless `--agents` or `--all-if-none` was given.

## sopc verify

Checks that what runs in production is what's in git: fetches each agent's live prompt from its platform and compares it with the compiled one, and checks that every tool its SOPs name exists on the live agent. Catches hotfixes made in a platform's dashboard, which the next deploy would silently revert, and tools that were renamed or removed. Run it nightly in CI; see the [drift check example](examples/drift-check).

```sh
sopc verify                 # every agent
sopc verify tonys-pizza     # only these agents
sopc verify --json          # for CI
```

```console
$ sopc verify
Verifying 4 agent(s) against their platforms
luigis-trattoria         elevenlabs  agent_7a1f...                in sync
  tools: missing lookup_allergens (required step); did you mean lookup_alergens?
  tools: not in any SOP: end_call, lookup_alergens
sakura-sushi             vapi        3f0c2d9e-...                 drifted
tonys-pizza              livekit     tonys-pizza                  skipped: LiveKit: your code loads the prompt and tools; nothing to fetch
pasta-bar                retell      agent_51c9...                error: RETELL_API_KEY is not set (needed to read Retell agents)

--- compiled/sakura-sushi.prompt.md
+++ live/vapi:3f0c2d9e-...
-Sakura is an omakase and sushi counter in Manhattan. Reservations strongly recommended. No delivery.
+Sakura is an omakase and sushi counter in Manhattan. Closed for a private event tonight. No delivery.

1 in sync, 1 drifted, 1 skipped, 1 error, 1 with missing tools
```

| Argument or flag | Effect |
|---|---|
| `IDS` | Agents to verify: sopc ids, platform ids or `platform:id` (default: every agent) |
| `--json` | Prints `{"ok", "summary", "agents": [{"id", "platform", "platform_id", "platform_ref", "status", "reason", "diff", "tools"}]}` instead |

The prompt is compiled from the current files in memory; `build/` isn't read. Live and compiled prompts must match exactly, except for line endings and whitespace at the very end. A drifted agent shows a diff from the compiled prompt (`-`) to the live one (`+`).

| Status | Meaning | Fails |
|---|---|---|
| `in sync` | The live prompt is the compiled one | |
| `drifted` | They differ; the diff is shown | yes |
| `not comparable` | The agent's prompt is split across nodes (an ElevenLabs workflow with subagent nodes, a Retell LLM with state prompts, a Retell conversation flow) or lives in a custom LLM server | |
| `skipped` | LiveKit agents: the prompt is loaded by your code, so there's nothing to fetch | |
| `error` | A key isn't set, the platform refused it, the agent wasn't found, or the platform couldn't be reached | yes |

**Tools.** For each agent, sopc collects the tools its SOPs name (`tool:` on steps, nevers and warning signs, groups expanded; a tool is `required` if a `required` step names it), plus `get_sop` when an SOP is delivered by tool, and looks for each one by name among the live agent's tools:

- **missing** (fails): named in an SOP but not on the platform. A live tool with nearly the same name (case, `_` vs `-`, or up to 2 edits) is suggested: `did you mean lookup_alergens?`.
- **not in any SOP** (information only): on the platform, but no SOP names it, e.g. `end_call`.
- **not confirmed** (doesn't fail): not listed, but the agent also has tools sopc can't list (MCP servers, Retell shared tools), so they may exist.

Tools are checked for `not comparable` agents too. LiveKit agents and Retell custom LLMs aren't checked: your code defines their tools. A tool that can't be looked up (e.g. a deleted tool id) is an error and fails.

| Platform | Where the tools are read |
|---|---|
| ElevenLabs | The agent response: `conversation_config.agent.prompt.tools[].name`, `built_in_tools` (system tools such as `end_call`, by name), and `tool_ids`, plus tool ids on workflow nodes (`additional_tool_ids`); each id is looked up with `GET /v1/convai/tools/{id}` (`tool_config.name`) |
| Vapi | The assistant response: `model.tools` (`function.name`, else `name`, else the type for built-ins such as `transferCall` and `endCall`), `endCallFunctionEnabled`; each of `model.toolIds` is looked up with `GET /tool/{id}` |
| Retell | The LLM verify already reads: `general_tools[].name` and `states[].tools[].name`. Conversation flows: `GET /get-conversation-flow/{id}?version=N` (the version the agent uses), every `tools[].name` in the flow, its components and nodes |

With `--json`, each agent's `tools` is `{"checked", "referenced": [{"name", "required"}], "missing": [{"name", "required", "suggestion"}], "unreferenced", "unconfirmed", "note", "error"}`, or `null` when the agent couldn't be read or is on LiveKit. `summary` adds `missing_tools` and `tool_errors` (counts of agents).

Exits 1 if any agent drifted, had an error, or is missing a tool. Only GET requests are sent; sopc never changes anything on a platform. Requests time out after 20 seconds.

| Variable | Used for |
|---|---|
| `ELEVENLABS_API_KEY` | ElevenLabs agents: `GET /v1/convai/agents/{id}`, prompt at `conversation_config.agent.prompt.prompt` |
| `VAPI_API_KEY` | Vapi assistants: `GET /assistant/{id}`, the `system` message in `model.messages` |
| `RETELL_API_KEY` | Retell agents: `GET /list-agent-versions/{id}` to find the newest published version, `GET /get-agent/{id}?version=N`, then `GET /get-retell-llm/{llm_id}?version=M` (the LLM version that agent version uses), `general_prompt` |
| `SOPC_ELEVENLABS_URL`, `SOPC_VAPI_URL`, `SOPC_RETELL_URL` | Another base URL for a platform's API, e.g. a proxy or a mock in tests (default: `https://api.elevenlabs.io`, `https://api.vapi.ai`, `https://api.retellai.com`) |

A key only needs read access.

Which version is compared:

- **Retell** keeps every version of an agent, and the newest one is often an unpublished draft. verify compares the newest published version (what Retell calls `latest_published`). An agent that was never published is compared by its newest draft, and the line says `not published on Retell; compared the latest draft`. A phone number can be pinned to an older version (`agent_version` in its `inbound_agents` or `outbound_agents`); calls to that number run the pinned version, which verify doesn't check.
- **ElevenLabs** returns the agent's saved configuration; unsaved drafts are per user and aren't included. With [versioning](https://elevenlabs.io/docs/eleven-agents/operate/versioning) on, sopc reads what ElevenLabs returns when no branch is given (the main branch); a share of traffic deployed to another branch isn't checked.
- **Vapi** has no version parameter on `GET /assistant/{id}`; it returns the current assistant. Unpublished dashboard drafts are per user and don't affect calls.

## sopc agents

Lists every agent: its id, platform, platform id and SOPs.

```sh
sopc agents
sopc agents --json   # adds blocks, instructions, tools and prompt hashes
```

With `--json`, each agent has `blocks` (instruction and SOP ids in prompt order, groups expanded), `instructions` and `sops` (the same, by kind), `tools` and `hash`.

| Flag | Effect |
|---|---|
| `--json` | Machine-readable output |

## sopc export

Prints every block as JSON, as sopc read it: for tools built on sopc, such as an editor or a server that shows the files as forms, so they don't need their own parser.

```sh
sopc export
sopc export -C path/to/sops
```

The output is `{"config", "instructions", "sops", "agents"}`. `config` holds `sopc.yaml`'s `variables`, `groups` and `sops_heading`, with defaults filled in. Each instruction, SOP and agent is the canonical JSON its `lock.json` hash is taken over (the fields in [FORMAT.md](FORMAT.md), with defaults filled in; a step written as plain text stays a string), plus `file`, its path in the folder. SOPs also get `rules`, as in `lock.json`. Agents also get `platform` and `platform_id`, and every platform field (`livekit`, `vapi`, `elevenlabs`, `retell`), `null` when unset. Variables, groups and `blocks` keep their order in the file.

Only parsing matters here: a file that can't be read prints `{"valid": false, "issues": [...]}`, as `sopc validate --json` does, and exits 1, but references between files (an unknown block, a missing variable) are left to `sopc validate`.

## sopc convert

Rewrites SOPs as Markdown or YAML. Converting never changes a compiled prompt; a file that can't be converted exactly is refused (`convert_failed`), e.g. a YAML goal with line breaks, which a Markdown field can't keep.

```sh
sopc convert --to md                # every YAML SOP
sopc convert --to yaml reservations # one SOP
```

| Argument or flag | Effect |
|---|---|
| `--to md\|yaml` | The format to write (required) |
| `IDS` | SOP ids to convert (default: every SOP not already in that format) |
| `-y`, `--yes` | Allow conversions that remove comments |

The new file is written before the old one is deleted. Comments at the top of a YAML file become front-matter comments, and back. Converting can change an SOP's hash in `lock.json` when only whitespace differs.

## sopc overlap

For importing existing prompts: shows the text that several prompts share, and near-copies: the same sentence with different words in some prompts. A near-copy whose differing words are values (a name, a place) is a placeholder candidate; one whose differing words change a rule is drift. With `--json` it also groups values into variables, and lists the text most prompts have but some lack and the text only one prompt has, for reviewing an import of many agents.

```sh
sopc overlap prompts/
sopc overlap prompts/ --json
sopc overlap prompts.json --json        # {"<agent-id>": "<prompt>", ...}
sopc overlap - --json < prompts.json    # the same JSON on stdin
```

| Argument or flag | Effect |
|---|---|
| `PROMPTS` | The existing prompts, one per agent: a folder of `<agent-id>.md` or `.txt` files, a JSON file holding an object of agent id → prompt text, or `-` to read that JSON from stdin |
| `--json` | Machine-readable output (below) |

Prompts are compared sentence by sentence (list items and lines count as sentences; headings and sentences under 3 words are skipped), by their words, ignoring case and punctuation. Sentences at least 75% alike are near-copies.

A near-copy's `kind` is `rule_word` when a differing word changes what a rule says, which is drift: decide which wording is right. Those words are:

- count words: `once`, `twice`, `thrice`, `one` to `ten`;
- negations (`not`, `never`, `no`, `don't`, `without`, ...) and modals (`always`, `must`, `may`, `should`, `can`, `only`, `required`, `optional`, ...);
- a whole number used as a count, limit, frequency or duration: after `up to`, `at most`, `at least`, `more than`, `within`, `under`, `over`, `every`, ... (`up to 3`, `within 5 minutes`), or before `times`, `minutes`, `hours`, `days`, `calls`, `items`, `per`, ... (`2 times`).

Any other differing text is a `value`, a placeholder candidate: names, and numbers that are identifiers, such as phone numbers (`555-0142`), prices (`$12.99`), times (`10pm`), street numbers, URLs, emails and ids. A value gets a `suggested_variable`, built from the content words just before it (else just after), e.g. `phone_number` for "Our phone number is 555-0142." Rename it as you like.

With `--json`, the output is one object:

| Field | Value |
|---|---|
| `agents` | Agent ids, sorted |
| `shared` | `[{"id", "text", "agents", "occurrences"}]`: sentences found in more than one prompt (ignoring case and punctuation), in first-seen order. `text` is the first agent's copy |
| `near_copies` | `[{"id", "kind", "agents_by_variant", "differing_tokens", "suggested_variable", "variable_id", "occurrences"}]`. `agents_by_variant` maps each wording to the agents that use it; `differing_tokens` maps each agent to its differing words; `suggested_variable` and `variable_id` (the `variables` entry it belongs to) are `null` for `rule_word` |
| `variables` | `[{"id", "suggested_name", "values", "occurrences"}]`: the `value` near-copies grouped by value, largest first. Near-copies whose values are the same for every agent they share (one's agents may be a subset of the other's) are one variable, e.g. a restaurant's name in ten sentences. `values` maps each agent to its value (the text between the words every variant starts and ends with); `occurrences` lists the near-copy ids. Every `value` near-copy is in exactly one variable |
| `missing` | `[{"id", "text", "present_in", "missing_in", "source"}]`: text that most prompts have (at least half, and at least two) but some don't. `missing_in` leaves out prompts that have a sentence at least 75% like it (they word it differently, a near-copy). `source` is `{"kind": "shared" or "near_copy", "id"}`, the group it comes from; for a near-copy, `text` is the most common wording. Most widely shared first |
| `unique` | `[{"agent", "spans": [{"start", "end", "text"}]}]`, one entry per agent: the text no other prompt has in any wording. Consecutive such sentences are one span, which includes what's between them (headings, line breaks) |
| `only_in_one` | Agent id → how many of its sentences no other prompt has |

Every `id` is 12 hex characters, a hash of the group's kind and normalized text (for a variable, of its near-copies' ids). The same text gets the same id in every run, whatever the agents are called or the order they're given in, so a review tool can keep notes on a group across runs. A near-copy's id changes when its set of wordings does.

Each occurrence is `{"agent", "start", "end", "text"}`: where the sentence is in that agent's prompt, so a UI can highlight it. `start` and `end` count characters (Unicode code points; for text outside the Basic Multilingual Plane, convert to UTF-16 units in JavaScript), with `end` exclusive, and `text` is exactly that slice. Near-copy occurrences add `differing: [{"start", "end", "text"}]`, the differing words, as offsets in the same prompt.

For these three prompts:

```text
luigis-trattoria.md:
  You are the phone host for Luigi's Trattoria.
  Speak warmly and briefly.
  Never upsell more than twice per call.
  Luigi's is a sit-down Italian restaurant in Queens.
  Thank you for calling Luigi's Trattoria, and have a lovely evening.

sakura-sushi.md:
  You are the phone host for Sakura Sushi.
  Speak warmly and briefly.
  Never upsell more than once per call.
  Thank you for calling Sakura Sushi, and have a lovely evening.

tonys-pizza.md:
  You are the phone host for Tony's Pizza.
  Never upsell more than once per call.
  Thank you for calling Tony's Pizza, and have a lovely evening.
```

the output is:

```json
{
  "agents": ["luigis-trattoria", "sakura-sushi", "tonys-pizza"],
  "shared": [
    {
      "id": "407ac29a8c0b",
      "text": "Speak warmly and briefly.",
      "agents": ["luigis-trattoria", "sakura-sushi"],
      "occurrences": [
        {"agent": "luigis-trattoria", "start": 46, "end": 71, "text": "Speak warmly and briefly."},
        {"agent": "sakura-sushi", "start": 41, "end": 66, "text": "Speak warmly and briefly."}
      ]
    }
  ],
  "near_copies": [
    {
      "id": "73540e3901e7",
      "kind": "value",
      "agents_by_variant": {
        "You are the phone host for Luigi's Trattoria.": ["luigis-trattoria"],
        "You are the phone host for Sakura Sushi.": ["sakura-sushi"],
        "You are the phone host for Tony's Pizza.": ["tonys-pizza"]
      },
      "differing_tokens": {"luigis-trattoria": "Luigi's Trattoria", "sakura-sushi": "Sakura Sushi", "tonys-pizza": "Tony's Pizza"},
      "suggested_variable": "phone_host",
      "variable_id": "2b4dc406a0e0",
      "occurrences": [
        {"agent": "luigis-trattoria", "start": 0, "end": 45, "text": "You are the phone host for Luigi's Trattoria.",
         "differing": [{"start": 27, "end": 44, "text": "Luigi's Trattoria"}]},
        {"agent": "sakura-sushi", "start": 0, "end": 40, "text": "You are the phone host for Sakura Sushi.",
         "differing": [{"start": 27, "end": 39, "text": "Sakura Sushi"}]},
        {"agent": "tonys-pizza", "start": 0, "end": 40, "text": "You are the phone host for Tony's Pizza.",
         "differing": [{"start": 27, "end": 39, "text": "Tony's Pizza"}]}
      ]
    },
    {
      "id": "b4d938405b24",
      "kind": "rule_word",
      "agents_by_variant": {
        "Never upsell more than twice per call.": ["luigis-trattoria"],
        "Never upsell more than once per call.": ["sakura-sushi", "tonys-pizza"]
      },
      "differing_tokens": {"luigis-trattoria": "twice", "sakura-sushi": "once", "tonys-pizza": "once"},
      "suggested_variable": null,
      "variable_id": null,
      "occurrences": [
        {"agent": "luigis-trattoria", "start": 72, "end": 110, "text": "Never upsell more than twice per call.",
         "differing": [{"start": 95, "end": 100, "text": "twice"}]},
        {"agent": "sakura-sushi", "start": 67, "end": 104, "text": "Never upsell more than once per call.",
         "differing": [{"start": 90, "end": 94, "text": "once"}]},
        {"agent": "tonys-pizza", "start": 41, "end": 78, "text": "Never upsell more than once per call.",
         "differing": [{"start": 64, "end": 68, "text": "once"}]}
      ]
    },
    {
      "id": "2962277e36e4",
      "kind": "value",
      "agents_by_variant": {
        "Thank you for calling Luigi's Trattoria, and have a lovely evening.": ["luigis-trattoria"],
        "Thank you for calling Sakura Sushi, and have a lovely evening.": ["sakura-sushi"],
        "Thank you for calling Tony's Pizza, and have a lovely evening.": ["tonys-pizza"]
      },
      "differing_tokens": {"luigis-trattoria": "Luigi's Trattoria", "sakura-sushi": "Sakura Sushi", "tonys-pizza": "Tony's Pizza"},
      "suggested_variable": "thank_calling",
      "variable_id": "2b4dc406a0e0",
      "occurrences": [
        {"agent": "luigis-trattoria", "start": 163, "end": 230, "text": "Thank you for calling Luigi's Trattoria, and have a lovely evening.",
         "differing": [{"start": 185, "end": 202, "text": "Luigi's Trattoria"}]},
        {"agent": "sakura-sushi", "start": 105, "end": 167, "text": "Thank you for calling Sakura Sushi, and have a lovely evening.",
         "differing": [{"start": 127, "end": 139, "text": "Sakura Sushi"}]},
        {"agent": "tonys-pizza", "start": 79, "end": 141, "text": "Thank you for calling Tony's Pizza, and have a lovely evening.",
         "differing": [{"start": 101, "end": 113, "text": "Tony's Pizza"}]}
      ]
    }
  ],
  "only_in_one": {"luigis-trattoria": 1, "sakura-sushi": 0, "tonys-pizza": 0},
  "variables": [
    {
      "id": "2b4dc406a0e0",
      "suggested_name": "phone_host",
      "values": {"luigis-trattoria": "Luigi's Trattoria", "sakura-sushi": "Sakura Sushi", "tonys-pizza": "Tony's Pizza"},
      "occurrences": ["73540e3901e7", "2962277e36e4"]
    }
  ],
  "missing": [
    {
      "id": "16db2ec35dbd",
      "text": "Speak warmly and briefly.",
      "present_in": ["luigis-trattoria", "sakura-sushi"],
      "missing_in": ["tonys-pizza"],
      "source": {"kind": "shared", "id": "407ac29a8c0b"}
    }
  ],
  "unique": [
    {"agent": "luigis-trattoria", "spans": [{"start": 111, "end": 162, "text": "Luigi's is a sit-down Italian restaurant in Queens."}]},
    {"agent": "sakura-sushi", "spans": []},
    {"agent": "tonys-pizza", "spans": []}
  ]
}
```

A `variables` entry is a placeholder to create once (`{{phone_host}}`, set per agent) and use in every sentence its `occurrences` point at. `missing` is often a rule some agents never got; `unique` is each agent's own text, a candidate for its `context` or an agent-specific instruction.

`overlap` finds near-copies through an index rather than by comparing every pair of sentences: 100 prompts of about 3,000 words each take well under a second.

`overlap` exits 0, unless the prompts can't be read (`no_prompts`: none found; `bad_prompts`: the JSON isn't an object of strings).

## sopc compare

For importing existing prompts: checks that each compiled prompt still says everything its original did. Exits 1 if a sentence was lost or changed, and shows the changed words. Sentences that were reworded (for example, one sentence split into steps) are listed but don't fail.

```sh
sopc compare --originals prompts/
sopc compare --originals prompts/ --json
sopc compare --originals originals.json --json   # {"<agent-id>": "<prompt>", ...}
sopc compare --originals - --json < originals.json
```

| Flag | Effect |
|---|---|
| `--originals DIR\|FILE\|-` | The original prompts: a folder of `<agent-id>.md` or `.txt` files, a JSON file holding an object of agent id → prompt text, or `-` to read that JSON from stdin (required) |
| `--json` | Machine-readable output (below). The exit code is the same |

Each original sentence is **kept** (found in the compiled prompt, at least 90% alike, or **reworded**: its words spread over several compiled sentences), **changed** (a compiled sentence at least half alike differs in some words) or **lost**.

**A changed rule word is always a change.** When the compiled sentence differs in a rule word, it is **changed** and compare fails, however alike the two are: a count or limit (`once` vs `thrice`, `up to 30 minutes` vs `up to 20 minutes`), a negation (a dropped `never`) or a modal (`must` vs `may`). These are the rule words of [`overlap`](#sopc-overlap). Other differences, such as values (`10pm` vs `11pm`, `$12.99` vs `$13.49`), are judged by how alike the sentences are, as before. Compiled sentences with no counterpart in the original are **added** (information only; sopc's own tool lines are left out).

With `--json`, the output is `{"summary": {"ok", "agents", "failed"}, "agents": [...], "no_original": [...]}`. `failed` counts agents with a lost or changed sentence; `no_original` lists compiled agents no original was given for. Each agent is:

| Field | Value |
|---|---|
| `agent`, `ok` | The agent id; `false` if a sentence was lost or changed |
| `sentences_total`, `kept`, `score` | Original sentences, how many were kept (reworded ones included), and `kept / sentences_total` (`1.0` when there are none) |
| `lost` | `[{"text"}]` |
| `changed` | `[{"original", "compiled", "changed_words": {"original", "compiled"}}]` |
| `reworded` | `[{"original", "compiled_candidates"}]`: the compiled sentences its words were spread over |
| `added` | `[{"text"}]` |

```json
{
  "summary": {"ok": false, "agents": 1, "failed": 1},
  "agents": [
    {
      "agent": "luigis-trattoria",
      "ok": false,
      "sentences_total": 8,
      "kept": 7,
      "score": 0.875,
      "lost": [],
      "changed": [
        {
          "original": "Never upsell more than twice per call.",
          "compiled": "Never upsell more than once per call.",
          "changed_words": {"original": "twice", "compiled": "once"}
        }
      ],
      "reworded": [],
      "added": [{"text": "Never upsell more than once per call."}]
    }
  ],
  "no_original": ["sakura-sushi", "tonys-pizza"]
}
```

## sopc migrate

Converts a folder written for an earlier sopc to the current format, in place. Other commands fail on such a folder with errors that say to run this.

- **sopc v0.0.8 and earlier** (`old_format` errors): bases and SOPs chose their agents (`agents`, `exclude`), agents inherited bases (`inherits`) and opted out (`exclude`), and `position` and `sop_order` set the order. Now each agent lists its blocks in order.
- **sopc v0.0.9** (`removed_field` errors): instructions and SOPs could set `locked: true`, so every agent had to include them. `locked` is gone; `migrate` deletes the line from each file that has it, and nothing else changes.

```sh
sopc migrate         # show the plan; write nothing
sopc migrate --yes   # write it
```

| Flag | Effect |
|---|---|
| `-y`, `--yes` | Write the changes (without it, only the plan is shown) |

What it does to a v0.0.8 folder:

- `bases/<id>.md` moves to `instructions/<id>.md`; `agents`, `exclude`, `inherits` and `position` are removed from its front matter (the front matter goes if nothing is left in it).
- SOPs lose `agents` and `exclude`; `sop_order` is removed from `sopc.yaml`.
- Each agent's `instructions` becomes `context`, written right after its platform id, followed by `blocks`: exactly the blocks the old rules gave it, in the order its old prompt had them, one per line. `inherits` and `exclude` are removed. No groups are created; add them afterwards where they help.
- `locked` is removed from every base and SOP. (A locked block could not be excluded, so those agents still get it in `blocks`.)
- Files are edited as text, so comments and layout are kept. Comments directly above a removed field (or on its line) go with it; the plan lists each one.

```console
$ sopc migrate
Migrating sops to the current format (13 file(s) change):
  sops/agents/la-casita.yaml: instructions → context; removed inherits; blocks: restaurant-host, brand-voice-es, allergen-check, delivery-handling, closing
  ...
  sops/bases/brand-voice.md → sops/instructions/brand-voice.md: removed agents, exclude, locked
  ...
...
Checked: all 4 agent prompt(s) stay the same, except each agent's own text (now `context`) moves to the top.
Nothing written. Rerun with --yes to write these changes.
```

**It checks before and after writing.** Every agent's prompt, `get_sop` payloads and tools are built from the migrated files and compared with what the old rules built: they must be identical, except that the agent's own text moves from after the top bases to the very top of the prompt. Each edited file is also read back to confirm it holds the same values minus the removed fields. If anything differs, nothing is written (`migrate_failed`). With `--yes`, each file is written through a temporary file and renamed into place, old `bases/` files are deleted last, and the check runs again on the files on disk. Then run `sopc` to rebuild `build/`: every hash in `lock.json` changes, because the block formats changed.

A v0.0.9 folder only loses its `locked` lines (and comments directly above them, which the plan lists):

```console
$ sopc migrate
Removing fields the format no longer has from sops (1 file(s) change):
  sops/instructions/brand-voice.md: removed locked
Checked: all 3 agent prompt(s) stay the same.
Nothing written. Rerun with --yes to write these changes.
```

A folder that's already partly migrated (it has `instructions/`, or an agent with `blocks` or `context`) is refused; finish it by hand. A folder in the current format prints `already in the current format` and exits 0. `plan` and `affected` build a git ref from before either change as migrating it would, so comparing with it works.

## sopc skills

Installs the `sopc-import` skill, which converts existing prompts into sopc files from inside your coding agent.

```sh
sopc skills install                  # Claude Code, Codex and OpenCode
sopc skills install --agent claude   # one agent
```

| Argument or flag | Effect |
|---|---|
| `install` | The only action |
| `--agent claude\|codex\|opencode` | Install for one agent; repeatable. Claude Code reads `.claude/skills`, Codex `.agents/skills`, OpenCode both |
| `--into FOLDER` | Install into another folder |

## sopc guide

Prints the format reference ([FORMAT.md](FORMAT.md)), so it's available offline and matches the installed version.

```sh
sopc guide | less
```
