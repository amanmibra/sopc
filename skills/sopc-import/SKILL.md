---
name: sopc-import
description: Convert existing voice/task agent prompts (in code, or pulled read-only from LiveKit, ElevenLabs, Vapi or Retell) into sopc files (shared instructions, SOPs, one file per agent listing the blocks it uses), verify nothing was lost, and report conflicts. Asks the user to fill gaps and approve a short import plan before writing files, and again before committing. Use when the user asks to import, migrate, modularize or "sopc-ify" agent prompts or instructions, or invokes this skill (/sopc-import in Claude Code, $sopc-import in Codex).
---

# Import existing prompts into sopc

You are turning a team's hand-written agent prompts into a sopc folder: shared text written once as instructions, procedures as SOPs, and a short file per agent with its own text and the list of blocks it uses, in prompt order. The compiled prompts must say everything the originals said. You do the judgment; the `sopc` CLI does the counting and checking; the user makes the decisions.

The flow has two approval checkpoints:

1. **Before writing any sopc file:** the user approves a one-screen import plan.
2. **Before committing or pushing:** the user approves a one-screen result summary.

Never commit, push or open a pull request without the second approval.

## How to ask questions

- Look first. Only ask what you can't find in the code, config or prompts.
- Batch questions: ask everything you need at one point, not one at a time. Use your question tool if you have one; otherwise a short numbered list.
- Offer a default with each question ("I'll use the LiveKit agent_name as the id unless you say otherwise"), so the user can answer "yes" to most of them.
- When the user doesn't know, take the option that keeps today's behavior, and note it in the plan.

## 0. Set up the CLI (automatic)

This skill needs the `sopc` command. Set it up without making the user do anything:

1. Run `sopc --version || sopc --help`. If it works, skip to step 3.
2. If it's missing, tell the user in one line that you're installing the sopc CLI (a single binary, into `~/.local/bin`), then run:

   ```
   curl -fsSL https://raw.githubusercontent.com/amanmibra/sopc/main/install.sh | sh
   ```

   If `sopc` still isn't found afterwards, run it by its full path (`~/.local/bin/sopc`) for the rest of this skill, and tell the user to add `~/.local/bin` to their PATH. If the install fails (no network, unsupported platform), stop and show the user the error.
3. Run `sopc guide` and read the whole format reference before writing any file.

## 1. Collect the originals

**First, ask where the agents run** (skip if the code already makes it obvious): LiveKit, Vapi, ElevenLabs, Retell, or prompts kept in code / files / a database. A team can use more than one.

Then get every agent's current prompt. Pull it from the platform when you can, so the user doesn't copy and paste. Only **read** from a platform: never create, update, publish or delete anything there.

| Platform | Where the prompts are | How to read them (first one that's set up) | Platform id for `agents/<id>.yaml` |
|---|---|---|---|
| LiveKit | In the agent's code (`instructions=`). Agent Builder agents: the user exports the code from the LiveKit Cloud dashboard. | Read the repo. | `livekit:` the `agent_name` |
| ElevenLabs | `conversation_config.agent.prompt.prompt` | 1. ElevenLabs MCP (`list_agents`, `get_agent`) if connected. 2. CLI: `npm i -g @elevenlabs/cli`, `elevenlabs auth login`, then `elevenlabs agents pull` in a scratch folder. 3. API: `GET https://api.elevenlabs.io/v1/convai/agents` and `/v1/convai/agents/<agent_id>` with header `xi-api-key: $ELEVENLABS_API_KEY`. | `elevenlabs:` the `agent_id` |
| Vapi | The `system` message in the assistant's `model.messages` | 1. Vapi MCP (`@vapi-ai/mcp-server`) if connected. 2. CLI: `vapi login`, `vapi assistant list`, `vapi assistant get <id>`. 3. API: `GET https://api.vapi.ai/assistant` and `/assistant/<id>` with `Authorization: Bearer $VAPI_API_KEY` (the private key). | `vapi:` the assistant id |
| Retell | Retell LLM `general_prompt` (plus `states[].state_prompt` for multi-state agents) | 1. Retell MCP if connected. 2. API: `GET https://api.retellai.com/list-agents` for the agent ids, then for each agent `GET /get-agent/<agent_id>?version=latest_published` (the version serving calls; without `version` you get the newest draft), and if its `response_engine.type` is `retell-llm`, `GET /get-retell-llm/<llm_id>?version=<response_engine.version>`, with `Authorization: Bearer $RETELL_API_KEY`. | `retell:` the `agent_id` |

**If nothing is set up, tell the user how, and wait.** Offer the easiest option for their tools:
- Claude Code with Retell: `claude mcp add --transport http retell https://mcp.retellai.com --header "Authorization: Bearer <RETELL_API_KEY>"`, then restart the session.
- ElevenLabs or Vapi: install the CLI and log in (commands above). Login opens a browser; the user runs it themselves (in Claude Code: `! elevenlabs auth login` or `! vapi login`).
- Otherwise, an API key in an environment variable (`ELEVENLABS_API_KEY`, `VAPI_API_KEY`, `RETELL_API_KEY`), set in the user's shell profile or a `.env` file that's gitignored. Then restart the session so you can see it.

Keys: never ask the user to paste a key into the chat, never print one, and never write one into a file you create. Read-only keys are enough if the platform offers them. If the user can't set any of this up, fall back to asking them to export or paste the prompts.

Some agents aren't one prompt: ElevenLabs workflows, Vapi squads or workflows, Retell conversation flows (`response_engine.type: conversation-flow`) and multi-state Retell LLMs. sopc compiles one prompt per agent, so list these in the plan as "not imported" with the reason, unless the user wants the main prompt imported alone. Also note per agent what lives outside the prompt (first message, voice, model, tools); sopc doesn't manage those, so leave them on the platform.

For each agent:
- Choose an id: lowercase kebab-case, ideally the platform's own name for it (LiveKit `agent_name`, or the agent's display name on other platforms).
- Copy the prompt **verbatim** into `sops/originals/<id>.md`. Resolve string concatenation or templating in code so the file is the text the model actually receives. Keep placeholders (e.g. `${restaurantName}`, `{{customer_name}}`) for now.
- Note the platform and platform id (see the table).
- Note the tool names the agent uses (in code, or the platform's tool list), so SOP steps can reference them.

**Ask now (one batch), only what you couldn't find:**
- Where prompts live, if you couldn't find all of them, and whether any agents should be left out (e.g. test or archived agents on the platform).
- Platform ids you couldn't find.
- Where the sopc folder should go (default: `sops/` at the repo root).
- Prompts assembled at runtime from data you can't see (e.g. a database): ask for an export or a sample.

## 2. Find what's shared

```
sopc overlap sops/originals
```

This lists sentences shared by all agents, shared by subsets, near-copies that differ only by a value, and how much is unique to each agent. Use it as the map for the plan. With many agents, read `sopc overlap sops/originals --json` instead: `variables` groups each value (a restaurant name used in ten sentences is one variable, with its value per agent), `missing` lists text most agents have but some lack, and `unique` gives each agent's own text.

## 3. Checkpoint 1: the import plan

Before writing any file in `sops/` (other than `sops/originals/`), show the user a plan that fits on one screen. Don't paste prompt text; name things and say who gets them. Example:

```
Import plan: 30 agents (LiveKit), sops/ at the repo root

Instructions (5)
  restaurant-host   identity + what the host does          all 30
  brand-voice       tone, one question at a time           all 30
  pizza-context     sizes, toppings, gluten-free note      12 pizza agents
  allergy-policy    allergen disclaimer                    all 30
  closing           repeat total and time before hanging up all 30, at the end

SOPs (4)
  allergen-check    all 30      steps use lookup_allergens
  reservations      18 agents
  large-orders      12 agents
  delivery          26 agents   (4 don't deliver)

Groups (2)
  core              restaurant-host, brand-voice, allergy-policy   all 30, first
  ordering          large-orders, delivery                          12 agents

Per agent: restaurant_name, hours, menu link become variables; 2–6 sentences stay in each agent's own context (first in its prompt).

Decisions needed
  1. Upsell rule differs: 29 agents say "once", luigis-trattoria says "twice". Unify on "once"? (default: keep both, as a variable, and flag it)
  2. No stated goal for reservations and large-orders. Use these? (default: yes)
       reservations: "The caller has a confirmed table, or knows why one isn't available."
       large-orders: "The order is placed with a pickup time the kitchen can meet."
  3. Passages that may or may not be procedures: "late-night menu" in 3 agents. SOP or plain text? (default: plain text)
```

Then ask: **"OK to write these files? Answer the numbered decisions or say 'defaults'."** Wait for the answer. Apply their changes to the plan before writing.

What belongs in "Decisions needed":
- **Drift.** The same sentence with different values across agents. The default keeps each agent's current behavior (a variable with each agent's value) and records it as a conflict. Unify only if the user says so.
- **Goals you'd have to invent.** Show the exact sentence you'd add.
- **Unclear classification.** Passages that could be a procedure or plain text.
- **Contradictions you already see.** Report them, don't resolve them.

## 4. Write the sopc files

Create `sops/sopc.yaml`, `sops/instructions/`, `sops/procedures/`, `sops/agents/`, following the approved plan and the format reference.

Blocks (instructions and SOPs) never say which agents use them. Each agent file lists its blocks, in the order the text appeared in that agent's original prompt:

| Text in the originals | Goes to |
|---|---|
| Shared, not a procedure (identity, tone, policies) | An instruction, `sops/instructions/<id>.md` (plain Markdown, no front matter needed) |
| A procedure: a situation and what to do in it | An SOP (see below) |
| The same blocks, in the same order, in many agents | A group in `sopc.yaml`; those agents list the group in `blocks` instead |
| A near-copy that differs only by a value (name, hours, phone number, link) | One shared sentence with a `{{placeholder}}`; each agent's value in its `variables` |
| Said only at the end of the call | An instruction each agent lists last |
| Facts about one agent only | That agent's `context` |

**Agents.** One file per agent, `sops/agents/<id>.yaml`: the platform id, then `context`, then `blocks` as a bullet list, one id per line, then `variables`:

```yaml
livekit: tonys-pizza
context: |
  Tony's is a wood-fired pizza shop in Brooklyn. Pickup only after 10pm.
blocks:
  - core
  - pizza-context
  - allergen-check
  - ordering
  - closing
variables:
  restaurant_name: Tony's Pizza
```

`context` goes first in the compiled prompt, before every block. If an original prompt had agent-specific text in the middle, it moves to the top; `compare` treats that as kept, but mention it in the summary. Each block may appear once per agent (directly or through a group).

**Groups.** In `sops/sopc.yaml`, also as bullet lists:

```yaml
groups:
  core:
    - restaurant-host
    - brand-voice
    - allergy-policy
```

Use a group only for a run of blocks that many agents share in the same order; don't force one.

**SOPs.** A passage is a procedure when it describes a situation and the steps to handle it ("If the caller wants to book a table, ask for…"). Write each one as Markdown, `sops/procedures/<id>.md`:

```markdown
# Reservations

**Goal:** The customer has a confirmed table, or knows exactly why one isn't available.
**When:** The customer wants to book, change or cancel a table.

Any other text from the passage goes here, as guidance.

## Steps
1. Ask for party size, date and time
2. Check availability `tool: check_reservations`

## Never
- Never double-book a table

## Warning signs
- The caller wants to book for more than 12; transfer to {{staff_transfer}} `tool: transfer_to_staff`
```

- Front matter: settings only, and only if needed (`locked`, `delivery`). There is no `agents` field: the agents that had this procedure list it in their `blocks`.
- `# ` name: a short title, the first line after the front matter.
- `**When:**` the situation that triggers it, in the original's words. `**Goal:**` the goal: the original's words if it states one, otherwise the sentence the user approved.
- `## Steps` (required, at least one): the ordered actions, in the original wording. `## Never`: "never / don't / do not" rules for this procedure. `## Warning signs`: "if the caller says X, transfer / escalate" rules. Only list items under these.
- If a step uses a tool, end it with `` `tool: <name>` `` using the tool's exact name from the agent code; add `` `required` `` if the call must always happen.
- Use exactly these names; anything else (`**Objective:**`, `## Notes`, `###` headings) is an error.

**Rules while writing:**
- Preserve wording. Don't improve, shorten or merge instructions during import; that's a separate, reviewed step. Allowed edits: replacing a value with a `{{placeholder}}`, and removing a sentence that a shared block now provides.
- Don't drop anything. If you can't place a sentence, put it in that agent's `context`.
- In YAML files (agents, `sopc.yaml`), quote any string containing `: ` (colon space), or use a `|` block.
- Convert code placeholders like `${restaurantName}` to `{{restaurant_name}}` with a value per agent.

If something comes up that the plan didn't cover and that changes what an agent would say, stop and ask before continuing.

## 5. Validate, compile, compare. Repeat until clean.

Run these from the repo root. They use `sops/` by default; if the folder is somewhere else, add `--dir path/to/sops` to each command (`sopc --dir path/to/sops`, `sopc validate --dir path/to/sops`).

```
sopc fmt
sopc validate
sopc
sopc compare --originals sops/originals
```

If `sopc fmt` lists a file it won't rewrite because that would remove comments, don't add `--yes`: leave the file, or move the comment's content somewhere it survives, and mention it to the user. Only use `--yes` if the user approves removing exactly those comments.

`compare` fails if any sentence from an original is **missing** or **changed** in the compiled prompt, and shows the changed words. Fix those; they're lost or altered instructions. **Reworded** lines (most words present, e.g. one sentence split into steps) don't fail; check each one says the same thing. **Added** lines should only be SOP headings and approved goals. Repeat until every agent passes.

## 6. Check for conflicts

```
sopc lint
```

This finds duplicated sentences, the same sentence with different numbers, "do X" vs "never X", near-identical sentences that drifted, and unused variables. Then read each compiled prompt in `sops/build/` yourself for contradictions the CLI can't see (e.g. one block offers delivery while the agent says pickup only).

**Don't fix conflicts silently.** List them for the user.

## 7. Checkpoint 2: the result summary

Run `sopc fmt` once more (then `sopc` if it changed anything) so the files are in canonical style. Then show one screen and ask before committing:

```
Imported 30 agents into sops/

  5 instructions, 4 SOPs, 2 groups, 30 agent files
  compare: all 30 agents pass (0 missing, 0 changed; 41 sentences reworded into SOP steps)
  added: 2 goals you approved

Conflicts to decide later (unchanged from today's behavior)
  1. upsell: "once" (29 agents) vs "twice" (luigis-trattoria), kept as a variable
  2. tonys-pizza says "pickup only after 10pm"; pizza-context says delivery until 11pm

Files: sops/sopc.yaml, sops/instructions/ (5), sops/procedures/ (4), sops/agents/ (30), sops/build/ (generated)
Not included: sops/originals/ (your old prompts, for reference; delete or add to .gitignore)

Commit these on a new branch and open a PR? (default: commit on branch sopc-import, don't push)
```

Only commit, push or open a PR after the user says so, and only as far as they said (commit only, push, or PR). Don't include `sops/originals/` unless asked.

Then list the next steps:
- Load each agent's prompt from `sops/build/<id>.prompt.md` in the agent code instead of the hard-coded string.
- Before future edits, run `sopc plan` (it compares with the default branch) to see which agents a change reaches.
