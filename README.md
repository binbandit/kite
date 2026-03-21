# 🪁 Kite (`kt`)

**Zero-thought continuous synthesis version control.**

Most Git tools are just wrappers. They put a colorful UI over the same old mental model: staging files, writing WIP commits, and managing branch state. 

Kite abandons manual versioning entirely. You write the code. Kite writes the history.

It acts as an instantaneous quicksave while you are in the flow state, and intelligently synthesizes your messy saves into a pristine, semantic Git history only when you are ready to share it.



## The Paradigm: Quicksaves and Landing

As a power user, your workflow is binary: you are either **in the zone**, or you are **delivering**. Kite isolates these two states completely.

### 1. In the Zone: The Quicksave
When you are coding, you don't stage files. You don't write commit messages. You just type:
```bash
kt

```

That’s it. By default, Kite instantly stages everything and creates a silent snapshot (`[kite] save 14:02:37`). It executes in milliseconds. You literally cannot lose work, and your flow state is never broken.

If you have already staged a specific subset yourself, Kite respects that selection and quicksaves only the staged changes. That path is there for deliberate exceptions; the normal recommended workflow is still to let Kite capture everything for you.

Quicksaves intentionally skip Git hooks to stay instant. Landing commits use normal `git commit` behavior, so your repository's configured Git hooks still run before polished history is written.

### 2. Delivering: Semantic Auto-Staging

When you are done with a feature, your history is full of messy quicksaves. You type:

```bash
kt land

```

Kite does not just squash your commits. It rewinds your quicksaves, analyzes the total diff of what you built, and tries providers in order: local Ollama, then the OpenAI Responses API, then a manual fallback prompt if AI is unavailable.

It automatically stages specific files together and writes atomic, Conventional Commits for each logical group:

* `feat(api): add stripe webhook endpoints` *(contains the 3 backend files)*
* `feat(ui): build checkout modal component` *(contains the 5 frontend files)*

**Zero thought. Perfect history. No errors.**

---

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

Starts a new flow. Kite checks for `main` first, otherwise `master`. If `origin` exists, it fetches that default branch and creates your new branch from `origin/<default>` when possible, otherwise from the local default branch.

```bash
kt go stripe-webhooks

```

### `kt` (Save)

The zero-friction quicksave. Run this constantly while you work.

- If the worktree is clean, Kite exits without creating a commit.
- If you already staged a deliberate subset, Kite quicksaves only that staged selection.
- Otherwise Kite runs `git add -A` and snapshots tracked plus untracked changes.
- Quicksaves use `--no-verify`, so hooks stay out of the way while you are in the flow.

```bash
kt

```

### `kt land`

Synthesizes contiguous Kite quicksaves into a polished history.

- Requires an existing `HEAD` commit.
- Rewinds only contiguous `[kite] save` commits at the top of history.
- Re-stages the full diff, groups files into logical commits, then creates normal `git commit`s so hooks do run during landing.
- If a remote exists, Kite first tries `git pull --rebase origin <current-branch>`, then pushes with `--set-upstream origin <current-branch> --force-with-lease`.

```bash
kt land

```

### `kt undo`

Attempts to restore the pre-land state.

- Requires a clean working tree.
- Hard-resets to `refs/kite/pre_land` and force-pushes if a remote exists.
- Current caveat: the CLI checks for `refs/kite/pre_land`, but the current `kt land` implementation does not obviously write that ref. Verify your build records it before relying on `kt undo` as a recovery path.

```bash
kt undo

```

---

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

If neither AI provider is available, Kite drops into a single commit-message prompt and lands the staged diff manually.

## Why Kite is Safe

Kite is built to be utterly bulletproof:

* **Branch Agnostic:** Kite uses a contiguous log walker. When you run `kt land`, it only rewinds continuous `[kite] save` commits. It will never overwrite a coworker's commits or touch your pre-existing history.
* **No Lost Files:** Even if the AI hallucinates, Kite cross-references the AI's output with your *actual* changed files. If the AI misses a file, Kite sweeps it up into a fallback commit. Absolutely no code is ever lost or left behind.
* **Remote Safety:** When publishing, Kite tries to rebase on the current remote branch first and then pushes with `--force-with-lease`, not a blind force-push.

---

*Built for developers who just want to write code.*
