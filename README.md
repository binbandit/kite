# 🪁 Kite (`kt`)

**Fast quicksaves and inspectable AI-assisted landing for Git.**

Kite splits version control into two phases:

- `kt` creates instant `[kite] save` snapshots while you are coding.
- `kt land` rewrites contiguous Kite saves into reviewable commits.
- `kt publish` pushes the rewritten branch when you are ready.
- `kt pr` opens a GitHub pull request for the landed branch with `gh`.

`kt go` is optional. It creates and checks out a fresh branch for a new piece of work, or switches to the named branch when it already exists. If you are already on the branch you want, keep using `kt`, `kt land`, and `kt publish` there.

The tool is intentionally opinionated about safety:

- landing only operates on committed Kite saves
- the plan is shown before history is rewritten
- landing is local by default
- every land records a rollback marker for `kt undo`

## The Workflow

### 1. Quicksave While You Work

When you are coding, run:

```bash
kt
```

By default, Kite stages everything and creates a snapshot like `[kite] save 14:02:37`.

If you already staged a deliberate subset yourself, Kite respects that selection and saves only the staged changes. That path is there for exceptions; the normal workflow is still to let Kite capture everything for you.

Quicksaves intentionally skip Git hooks to stay fast. Landed commits use normal `git commit` behavior, so hooks do run when polished history is written.

### 2. Land Saved Work Into Reviewable Commits

When your branch is full of contiguous Kite saves, run:

```bash
kt land
```

Kite analyzes the diff introduced by those saves, proposes logical commit groups, and only rewrites history after you confirm the plan.

When AI is available, Kite tries providers in this order:

1. Local Ollama
2. OpenAI Responses API
3. Manual fallback prompt

Kite also feeds recent non-Kite commit messages from the current repository into the prompt so landed messages follow the repo's existing style when possible. If the repo does not show a clear pattern, Kite falls back to Conventional Commit style.

Typical landed output looks like:

- `feat(api): add stripe webhook endpoints`
- `feat(ui): build checkout modal component`

If AI is unavailable, Kite shows the provider failures and asks for one manual commit message before any history is changed.

### 3. Open a Pull Request

Once the branch is landed, run:

```bash
kt pr
```

Kite gathers everything a good pull request needs, drafts it with the same AI cascade, shows you the result, and only creates it after you confirm. See [`kt pr`](#kt-pr) below for details.

## Installation

Ensure you have Rust installed, then build and install the binary globally:

```bash
git clone https://github.com/binbandit/kite.git kite
cd kite
cargo install --path .
```

## Install The Agent Skill

This repo also ships an installable agent skill at `skills/use-kite`.

Install it from GitHub with the `skills` CLI:

```bash
npx skills add https://github.com/binbandit/kite --skill use-kite
```

Install it specifically for Codex:

```bash
npx skills add https://github.com/binbandit/kite --skill use-kite -a codex
```

Install it globally so it is available across projects:

```bash
npx skills add https://github.com/binbandit/kite --skill use-kite -a codex -g
```

If you are testing from a local checkout, install from the current directory instead:

```bash
npx skills add . --skill use-kite -a codex
```

## Usage

### `kt go <idea>`

Creates and checks out a new flow branch. If the branch already exists locally, Kite switches to it and prints a note that no new branch was created. Use it when you want to start or resume a branch for a piece of work. If you are already on the right branch, skip this command entirely.

`kt go` does not change how landing works. After it switches branches, you keep working normally with `kt`, `kt land`, and `kt publish` on that branch.

Kite prefers `origin/HEAD` when it exists, otherwise falls back to `main`, `master`, or the current branch.

```bash
kt go stripe-webhooks
```

### `kt`

The zero-friction quicksave. Run this constantly while you work.

- If the worktree is clean, Kite exits without creating a commit.
- If you already staged a deliberate subset, Kite quicksaves only that staged selection.
- Otherwise Kite runs `git add -A` and snapshots tracked plus untracked changes.
- Quicksaves use `--no-verify`, so hooks stay out of the way while you are in the flow.

```bash
kt
```

### `kt land`

Synthesizes contiguous Kite quicksaves into a polished local history.

- Requires an existing `HEAD` commit.
- By default, requires a clean working tree. If you still have WIP changes, run `kt` first or stash them.
- Use `--allow-dirty` to land while your worktree is dirty; `kt` temporarily stashes and restores those changes.
- Only rewrites contiguous `[kite] save` commits at the top of history.
- Shows the proposed commit plan before rewriting anything.
- Stores the pre-land `HEAD` in `refs/kite/pre_land` so `kt undo` can restore it later.
- Creates normal `git commit`s, so hooks do run during landing.
- If landing fails after rewriting starts, Kite keeps the partial state on a `kite-recovery-*` branch so partial landed commits or staged changes are not lost.
- Lands locally by default. Use `kt publish` afterward, or pass `--push` to publish immediately after landing.

```bash
kt land
```

Skip the confirmation prompt:

```bash
kt land --yes
```

Land without a clean worktree:

```bash
kt land --allow-dirty
```


Land and publish in one step:

```bash
kt land --push
```

### `kt publish`

Publishes the current branch after you review the rewritten local history.

- Pushes with `--set-upstream origin <current-branch> --force-with-lease` — and nothing else.
- Deliberately no `git pull --rebase` first: after a land, the remote still holds the old saves, and rebasing onto them would resurrect the history you just rewrote.
- If someone else pushed to the branch, the lease rejects the push and Kite reports it; you decide how to reconcile.
- If no remote exists, Kite exits without error and leaves the history local.

```bash
kt publish
```

### `kt pr`

Opens a GitHub pull request for the current branch using the [GitHub CLI](https://cli.github.com) (`gh`). It is deliberately smart about the draft:

- Requires `gh` to be installed and authenticated, and a remote to exist.
- Refuses to run with unlanded saves so the pull request always shows polished commits — run `kt land` first.
- Publishes the branch automatically when the remote is missing it or behind it.
- If a pull request is already open for the branch, prints its URL and stops.
- Finds the repository's pull request template in the places GitHub looks (`.github/`, the repo root, `docs/`, and `.github/PULL_REQUEST_TEMPLATE/`) and fills it in.
- Discovers PR-related agent skills installed on the machine (`.claude/skills`, `.agents/skills`, and `skills` in the repo; `~/.claude/skills`, `~/.codex/skills`, and `~/.agents/skills` per user) and feeds their guidance to the AI.
- Uses recent merged pull request titles from the repository as style examples for the new title.
- Drafts the title and body with the same Ollama → OpenAI cascade as `kt land`. Without AI, it falls back to a deterministic draft built from the template and the branch's commits.
- Always previews the pull request and asks for confirmation before creating anything.

```bash
kt pr
```

Create it as a draft, target a different base branch, or skip the confirmation:

```bash
kt pr --draft
kt pr --base release/1.2
kt pr --yes
```

### `kt undo`

Attempts to restore the pre-land state.

- Requires a clean working tree.
- Hard-resets to `refs/kite/pre_land` and force-pushes if a remote exists.
- Deletes the rollback marker after a successful undo so you do not accidentally replay it twice.

```bash
kt undo
```

## Configuration & AI Providers

Kite uses a local-first cascade for both `kt land` and `kt pr`, and falls back without blocking either flow.

**1. Local Ollama**

- Endpoint: `http://localhost:11434/api/chat`
- Model env: `KITE_LOCAL_MODEL`
- Timeout env: `KITE_LOCAL_TIMEOUT_SECS`
- Default model: `llama3`
- Default timeout: 30 seconds

```bash
export KITE_LOCAL_MODEL="llama3"
export KITE_LOCAL_TIMEOUT_SECS="30"
```

**2. OpenAI Responses API**

- Base URL env precedence: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model env precedence: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key env precedence: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`
- Timeout env: `KITE_OPENAI_TIMEOUT_SECS`
- Default base URL: `https://api.openai.com/v1`
- Default model: `gpt-5.4-mini`
- Default timeout: 120 seconds
- Kite normalizes base URLs that end in `/responses`, `/chat/completions`, or omit `/v1`

```bash
export OPENAI_API_KEY="sk-..."
export KITE_OPENAI_MODEL="gpt-5.4-mini"
export KITE_OPENAI_TIMEOUT_SECS="120"
```

**3. Manual fallback**

If neither AI provider is available, Kite shows the provider failures and keeps working: `kt land` asks for one manual commit message (leaving it blank aborts without changing history), and `kt pr` builds a deterministic draft from the template and the branch's commits.

## Why Kite Is Safe

Kite keeps the risky parts explicit:

- **Clean-worktree landing by default:** `kt land` refuses to run with staged or unstaged WIP, so scratch files do not get swept into a landed commit by surprise.
- **Dirty worktree override:** `kt land --allow-dirty` temporarily stashes uncommitted changes, lands saved commits, then restores those changes.
- **Preview before rewrite:** Kite shows the proposed commit plan before it rewrites history.
- **Rollback marker:** Every successful land records the previous `HEAD` at `refs/kite/pre_land`.
- **No dropped files:** If the AI misses files, Kite adds a `chore: unclassified updates` commit rather than silently omitting them.
- **Explicit publish:** Landing is local by default. Publishing remains a separate step unless you opt into `--push`.
- **Preview before PR:** `kt pr` shows the full title and body and asks for confirmation before anything reaches GitHub.

---

*Built for developers who want cheap quicksaves and cleaner review history.*
