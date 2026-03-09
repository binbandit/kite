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

That’s it. By default, Kite instantly stages everything and creates a silent snapshot (`[kite] save 14:02`). It executes in milliseconds. You literally cannot lose work, and your flow state is never broken.

If you have already staged a specific subset yourself, Kite respects that selection and quicksaves only the staged changes. That path is there for deliberate exceptions; the normal recommended workflow is still to let Kite capture everything for you.

Quicksaves intentionally skip Git hooks to stay instant. Landing commits use normal `git commit` behavior, so your repository's configured Git hooks still run before polished history is written.

### 2. Delivering: Semantic Auto-Staging

When you are done with a feature, your history is full of messy quicksaves. You type:

```bash
kt land

```

Kite does not just squash your commits. It rewinds your quicksaves, analyzes the total diff of what you built, and uses local AI to **logically group your changed files**.

It automatically stages specific files together and writes atomic, Conventional Commits for each logical group:

* `feat(api): add stripe webhook endpoints` *(contains the 3 backend files)*
* `feat(ui): build checkout modal component` *(contains the 5 frontend files)*

**Zero thought. Perfect history. No errors.**

---

## Installation

Ensure you have Rust installed, then build and install the binary globally:

```bash
git clone <your-repo-url> kite
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

Starts a new flow. Pulls the latest default branch, cuts a new feature branch, and gets out of your way.

```bash
kt go stripe-webhooks

```

### `kt` (Save)

The zero-friction quicksave. Run this constantly while you work. It captures all tracked and untracked changes instantly.

```bash
kt

```

### `kt land`

Synthesizes your quicksaves into a pristine history and force-pushes to your remote.

```bash
kt land

```

---

## Configuration & AI Providers

Kite uses a Local-First architecture with graceful cloud degradation. It will never block you.

**1. Local First (Recommended)**
For zero-latency, private synthesis, install [Ollama](https://ollama.com/) and pull a lightweight model like `llama3`. Kite will automatically detect and use it.

```bash
export KITE_LOCAL_MODEL="llama3" # Default

```

**2. Cloud Fallback (OpenAI)**
If your local model is offline, Kite seamlessly falls back to OpenAI.

```bash
export OPENAI_API_KEY="sk-..."

```

**3. Manual Fallback**
If you have no internet and no local model, Kite instantly drops you into a minimal manual prompt. You are never blocked from landing your code.

## Why Kite is Safe

Kite is built to be utterly bulletproof:

* **Branch Agnostic:** Kite uses a contiguous log walker. When you run `kt land`, it only rewinds continuous `[kite] save` commits. It will never overwrite a coworker's commits or touch your pre-existing history.
* **No Lost Files:** Even if the AI hallucinates, Kite cross-references the AI's output with your *actual* changed files. If the AI misses a file, Kite sweeps it up into a fallback commit. Absolutely no code is ever lost or left behind.

---

*Built for developers who just want to write code.*
