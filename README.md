# 🪁 Kite (`kt`)

**Fast quicksaves and inspectable AI-assisted landing for Git.**

Kite splits version control into two phases:

- `kt` creates instant `[kite] save` snapshots while you are coding.
- `kt land` rewrites contiguous Kite saves into reviewable commits.
- `kt publish` pushes the rewritten branch when you are ready.

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

Starts a new flow. Kite prefers `origin/HEAD` when it exists, otherwise falls back to `main`, `master`, or the current branch.

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
- Requires a clean working tree. If you still have WIP changes, run `kt` first or stash them.
- Only rewrites contiguous `[kite] save` commits at the top of history.
- Shows the proposed commit plan before rewriting anything.
- Stores the pre-land `HEAD` in `refs/kite/pre_land` so `kt undo` can restore it later.
- Creates normal `git commit`s, so hooks do run during landing.
- Lands locally by default. Use `kt publish` afterward, or pass `--push` to publish immediately after landing.

```bash
kt land
```

Skip the confirmation prompt:

```bash
kt land --yes
```

Land and publish in one step:

```bash
kt land --push
```

### `kt publish`

Publishes the current branch after you review the rewritten local history.

- If a remote exists, Kite first tries `git pull --rebase origin <current-branch>`.
- Then it pushes with `--set-upstream origin <current-branch> --force-with-lease`.
- If no remote exists, Kite exits without error and leaves the history local.

```bash
kt publish
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

Kite uses a local-first cascade and falls back without blocking the landing flow.

**1. Local Ollama**

- Endpoint: `http://localhost:11434/api/chat`
- Model env: `KITE_LOCAL_MODEL`
- Default model: `llama3`

```bash
export KITE_LOCAL_MODEL="llama3"
```

**2. OpenAI Responses API**

- Base URL env precedence: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model env precedence: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key env precedence: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`
- Default base URL: `https://api.openai.com/v1`
- Default model: `gpt-5.4-mini`
- Kite normalizes base URLs that end in `/responses`, `/chat/completions`, or omit `/v1`

```bash
export OPENAI_API_KEY="sk-..."
export KITE_OPENAI_MODEL="gpt-5.4-mini"
```

**3. Manual fallback**

If neither AI provider is available, Kite shows the provider failures and asks for one manual commit message. Leaving the prompt blank aborts the land without changing history.

## Why Kite Is Safe

Kite keeps the risky parts explicit:

- **Clean-worktree landing:** `kt land` refuses to run with staged or unstaged WIP, so scratch files do not get swept into a landed commit by surprise.
- **Preview before rewrite:** Kite shows the proposed commit plan before it rewrites history.
- **Rollback marker:** Every successful land records the previous `HEAD` at `refs/kite/pre_land`.
- **No dropped files:** If the AI misses files, Kite adds a `chore: unclassified updates` commit rather than silently omitting them.
- **Explicit publish:** Landing is local by default. Publishing remains a separate step unless you opt into `--push`.

---

*Built for developers who want cheap quicksaves and cleaner review history.*
