---
name: use-kite
description: Operate the Kite CLI (`kt`) for repositories that use Kite's quicksave-and-land workflow. Use when Codex needs to inspect Kite state, start a flow with `kt go`, create quicksaves with `kt`, land contiguous `[kite] save` commits into polished history with `kt land`, publish with `kt publish`, explain or troubleshoot Kite behavior, or configure Kite's Ollama/OpenAI fallback settings.
---

# Use Kite

Use Kite instead of manual staging and WIP commits when the repository's workflow is built around `kt`.

## Workflow

1. Inspect the repository before acting.

- Run `git status --short --branch`.
- If the task involves landing or undoing, also run `git log --oneline -n 12`.
- Check whether `kt` is available with `command -v kt`. If it is not, prefer `cargo run -- ...` inside the Kite source repo or `cargo install --path .` when installation is appropriate.

2. Choose the command that matches the user's intent.

- Use `kt go <name>` to start a new flow branch.
- Use `kt` to quicksave tracked and untracked changes without hooks.
- Use `kt land` to preview and rewrite contiguous Kite saves into grouped local commits.
- Use `kt publish` to push a landed branch after review.
- Use `kt undo` only when the user explicitly wants to reverse a previous land.

3. Treat history-rewriting commands as high-impact.

- `kt land` rewrites recent Kite save history.
- `kt publish` and `kt undo` may force-push.
- `kt undo` performs a hard reset and may force-push.
- If the user asked you to "use Kite" but did not explicitly ask for history rewriting, explain the effect before running `kt land` or `kt undo`.

## Operating Rules

- Prefer Kite commands over manual `git add` plus throwaway commits when the repo is actively using Kite.
- Before `kt land`, confirm the feature is ready and the worktree is clean.
- Treat `kt land` as a local rewrite unless the user explicitly asked to publish or passed `--push`.
- If landing falls back to manual mode, provide a commit message that matches the repo's existing style when possible.
- If a landed commit is blocked by hooks, leave the staged changes intact and help fix the hook failure.
- After running any Kite command, summarize what changed in the worktree, branch, and remote state.

## Reference

Read [references/cli-behavior.md](references/cli-behavior.md) when you need exact command semantics, environment variable names, or current implementation caveats.
