# 📋🧩 Example: a nightly drift check

A scheduled GitHub Action that fails when an agent's **live prompt** on its platform no longer matches the prompt **compiled from your repo**.

```
.github/workflows/drift-check.yml   copy into your repo, then add your platform keys as secrets
```

## Why

A prompt gets hotfixed in the ElevenLabs, Vapi or Retell dashboard at 11pm. Production now differs from git, and the next deploy from git silently reverts the fix. `sopc verify` catches it the next morning, with a diff of what was changed, so the fix can be copied into the sopc files (and reviewed) before anything ships over it.

## What it does

Every night (and when started by hand), on your default branch:

```sh
sopc verify
```

For each agent, sopc compiles its prompt from the files, fetches the live one with a read-only `GET`, and compares them. The run fails if any agent **drifted**, had an **error** (a missing or refused key, an agent not found on the platform), or is **missing a tool** one of its SOPs names. The output, with a diff for each drifted agent, goes to the log and the run's summary:

```
sakura-sushi             vapi        3f0c2d9e-...                 drifted

--- compiled/sakura-sushi.prompt.md
+++ live/vapi:3f0c2d9e-...
-Sakura is an omakase and sushi counter in Manhattan. Reservations strongly recommended. No delivery.
+Sakura is an omakase and sushi counter in Manhattan. Closed for a private event tonight. No delivery.

3 in sync, 1 drifted, 1 skipped
```

`-` lines are what git has; `+` lines are what's live. LiveKit agents are skipped (your code loads their prompt from the build), and agents whose prompt is split across nodes (ElevenLabs workflows with subagents, Retell state prompts or conversation flows) are reported as not comparable. Neither fails the run.

## Set it up

1. Copy `.github/workflows/drift-check.yml` into your repo.
2. Add a repository secret for each platform you use (Settings → Secrets and variables → Actions): `ELEVENLABS_API_KEY`, `VAPI_API_KEY`, `RETELL_API_KEY`. Read access is enough. Delete the lines for platforms you don't use.
3. If your sopc folder isn't `sops/`, set `SOPC_ROOT`. To pin sopc, set `SOPC_VERSION` to a release tag.
4. Run it once by hand from the Actions tab.

GitHub emails the workflow's owner when a scheduled run fails. To post to Slack or open an issue instead, add a step with `if: failure()`.

## When it fails

- **drifted:** someone changed the prompt on the platform. Either copy the change into the right instruction, SOP or agent file (`sopc plan` shows which agents it reaches) and deploy, or redeploy from git to undo it.
- **tools: missing:** an SOP names a tool the live agent doesn't have, often a rename (`did you mean ...?`). Fix the name in the SOP, or add the tool on the platform.
- **error:** the line names the cause, e.g. `VAPI_API_KEY is not set` or `not found on Vapi (HTTP 404)` for an agent deleted on the platform.

`sopc verify --json` prints the same report as JSON, for tools that post it elsewhere. Every flag is in the [CLI reference](../../CLI.md#sopc-verify).
