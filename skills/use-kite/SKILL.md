---
name: use-kite
description: Operate the Kite CLI (`kt`) for repositories that use Kite's quicksave-and-land workflow. Use when Codex needs to inspect Kite state, start a flow with `kt go`, create quicksaves with `kt`, land contiguous `[kite] save` commits into polished history with `kt land`, publish with `kt publish`, open a pull request with `kt pr`, explain or troubleshoot Kite behavior, or configure Kite's OpenAI settings.
---

# Use Kite

Use Kite instead of manual staging and WIP commits when the repository's workflow is built around `kt`.

## Workflow

1. Inspect the repository before acting.

- Run `git status --short --branch`.
- If the task involves landing or undoing, also run `git log --oneline -n 12`.
- Check whether `kt` is available with `command -v kt`. If it is not, prefer `cargo run -- ...` inside the Kite source repo or `cargo install --path .` when installation is appropriate.

2. Choose the command that matches the user's intent.

- Use `kt go <name>` only when the user explicitly wants a fresh branch or the current branch is clearly the wrong place to start the work.
- Use `kt` to quicksave tracked and untracked changes without hooks.
- Use `kt land` to preview and rewrite contiguous Kite saves into grouped local commits.
- Use `kt publish` to push a landed branch after review.
- Use `kt pr` to open a GitHub pull request for a landed branch. It requires the `gh` CLI, refuses to run with unlanded saves, and previews the AI-drafted title and body before creating anything. If the branch's pull request is already open, it pushes new commits and offers a refreshed body when the existing one has gone stale.
- Use `kt undo` only when the user explicitly wants to reverse a previous land.

3. Treat history-rewriting commands as high-impact.

- `kt land` rewrites recent Kite save history.
- `kt publish` and `kt undo` may force-push.
- `kt pr` publishes the branch when needed and creates a pull request on GitHub after user confirmation (or immediately with `--yes`).
- `kt undo` performs a hard reset and may force-push.
- If the user asked you to "use Kite" but did not explicitly ask for history rewriting, explain the effect before running `kt land` or `kt undo`.

## Operating Rules

- Prefer Kite commands over manual `git add` plus throwaway commits when the repo is actively using Kite.
- Before `kt land`, confirm the feature is ready and the worktree is either clean or explicitly handled by `--allow-dirty`.
- Do not call `kt go` automatically just because the user mentioned an issue or task. If they are already on the right branch, stay there.
- Treat `kt land` as a local rewrite unless the user explicitly asked to publish or passed `--push`.
- A detached `HEAD` is fine for `kt`, `kt land`, and `kt undo` — they move `HEAD` itself. Only `kt publish`, `kt pr`, and `kt land --push` need a branch, so create one with `git switch -c <name>` when the user actually wants to push.
- If landing falls back to manual mode, provide a commit message that matches the repo's existing style when possible.
- If a landed commit is blocked by hooks, leave the staged changes intact and help fix the hook failure.
- After running any Kite command, summarize what changed in the worktree, branch, and remote state.

## Reference

Read [references/cli-behavior.md](references/cli-behavior.md) when you need exact command semantics, environment variable names, or current implementation caveats.
