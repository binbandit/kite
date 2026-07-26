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

Grouping is hunk-level: when one file contains changes for different purposes, its hunks can land in different commits. The plan marks split files with `(1/2 hunks)`-style annotations. Before rewriting anything, Kite replays the hunk-level plan against a temporary index and requires the result to reproduce your saved state exactly; if it cannot, it lands whole files instead.

Kite also feeds recent non-Kite commit messages from the current repository into the prompt so landed messages follow the repo's existing style when possible. If the repo does not show a clear pattern, Kite falls back to Conventional Commit style.

Typical landed output looks like:

- `feat(api): add stripe webhook endpoints`
- `feat(ui): build checkout modal component`

If the AI is unavailable, Kite shows the failure and asks for one manual commit message before any history is changed.

### 3. Open a Pull Request

Once the branch is landed, run:

```bash
kt pr
```

Kite gathers everything a good pull request needs, drafts it with the same AI as `kt land`, shows you the result, and only creates it after you confirm. See [`kt pr`](#kt-pr) below for details.

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

Creates and checks out a new flow branch. If the branch already exists — locally, or only on the remote — Kite switches to it instead of creating anything. A branch that exists only on `origin` is checked out with tracking, so resuming work from another machine, or picking up a colleague's branch, keeps their commits instead of starting a divergent branch with the same name. Use it when you want to start or resume a branch for a piece of work. If you are already on the right branch, skip this command entirely.

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
- Saved one you did not mean to? `kt undo` puts it straight back in your working tree.

```bash
kt
```

### `kt land`

Synthesizes contiguous Kite quicksaves into a polished local history.

- Requires an existing `HEAD` commit.
- By default, requires a clean working tree. If you still have WIP changes, run `kt` first or stash them.
- Use `--allow-dirty` to land while your worktree is dirty; `kt` temporarily stashes and restores those changes.
- Requires a branch: on a detached `HEAD` Kite refuses before touching anything.
- Only rewrites contiguous `[kite] save` commits at the top of history, following first-parent history so a merge cannot move the starting point.
- Splits changes by hunk, so one file can contribute to multiple commits. Binary files, mode changes, and renames stay whole.
- Very large sets of saves group by file instead of by hunk, and say so. Past a few hundred hunks the model cannot be shown enough of each one to tell them apart, and grouping whole files it can actually read beats guessing at hunks it never saw.
- Verifies the hunk-level plan against a temporary index first; if the replayed commits would not reproduce your saved state bit-for-bit, Kite falls back to whole-file grouping.
- Shows the proposed commit plan before rewriting anything.
- Stores the pre-land `HEAD` in `refs/kite/pre_land` so `kt undo` can restore it later.
- Creates normal `git commit`s, so hooks do run during landing.
- Landing never creates a branch. It builds the new commits on a detached `HEAD` and only moves your branch once they all exist.
- If landing fails for any reason — a rejected pre-commit hook is the usual one — Kite undoes the attempt and leaves you exactly where you started: on your branch, saves intact, nothing staged, no branch to clean up. Fix the problem and run `kt land` again. Files a hook rewrote are kept as unstaged changes.
- If landing is interrupted rather than failing — Ctrl-C, a crash, a closed terminal — the next `kt` command notices and puts you back the same way.
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

- Fetches the branch, then pushes with `--set-upstream origin <current-branch>`.
- Forces only when it has to. If the remote is already an ancestor of your branch, the push is an ordinary fast-forward and nothing is forced.
- Deliberately no `git pull --rebase` first: after a land, the remote still holds the old saves, and rebasing onto them would resurrect the history you just rewrote.
- When the remote holds commits your branch does not, Kite looks at what a force would discard. Kite saves are the history you just rewrote, so those go without ceremony. Anything else is someone's work: Kite lists the commits and asks before touching them.
- If no remote exists, Kite exits without error and leaves the history local.

A bare `--force-with-lease` is not enough for this, which is why Kite does not rely on it alone: the lease compares against your local remote-tracking ref, and when that ref does not exist — the normal case for a branch someone else created — git has nothing to compare and lets the push through.

```bash
kt publish
```

### `kt pr`

Opens a GitHub pull request for the current branch using the [GitHub CLI](https://cli.github.com) (`gh`). It is deliberately smart about the draft:

- Requires `gh` to be installed and authenticated, and a remote to exist.
- Refuses to run with unlanded saves so the pull request always shows polished commits — run `kt land` first.
- Publishes the branch automatically when the remote is missing it or behind it.
- If a pull request is already open for the branch, Kite pushes any new commits, checks whether the body still reflects the branch, and offers a refreshed body when it doesn't — preserving the existing structure and any human-written notes. Without AI, the existing body is never touched.
- Finds the repository's pull request template in the places GitHub looks (`.github/`, the repo root, `docs/`, and `.github/PULL_REQUEST_TEMPLATE/`), fills it in, and drops sections that don't apply — no empty headings, no `N/A`, no leftover boilerplate.
- Discovers PR-related agent skills installed on the machine (`.claude/skills`, `.agents/skills`, and `skills` in the repo; `~/.claude/skills`, `~/.codex/skills`, and `~/.agents/skills` per user) and treats them as your own instructions for how the pull request must be written — they take precedence over the default rules.
- Uses recent merged pull request titles from the repository as style examples for the new title.
- Drafts the title and body with the same AI as `kt land`. Without AI, it falls back to a deterministic draft built from the template and the branch's commits.
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

Reverses the most recent thing Kite did — the last quicksave if there is one on top of history, otherwise the last land. Run it repeatedly to walk back through your saves and then the land beneath them, in the order they happened.

**Undoing a quicksave**

- Uncommits the save and puts its changes back in your working tree, exactly as they were before you ran `kt`.
- Never touches the working tree, so edits you made after the save survive and the tree does not need to be clean.
- Entirely local — nothing is pushed.

**Undoing a land**

- Requires a clean working tree.
- Only undoes the branch that was landed. Landing records which branch the rollback belongs to, so running `kt undo` from somewhere else refuses and tells you where to go — it cannot reset an unrelated branch to unrelated history.
- Asks first if the branch has moved since the land, since those newer commits would be discarded.
- Hard-resets to `refs/kite/pre_land` and force-pushes that branch if a remote exists.
- Deletes the rollback marker after a successful undo so you do not accidentally replay it twice.

```bash
kt undo
```

## Configuration & AI

Both `kt land` and `kt pr` use one AI, reached through the OpenAI Responses API, and fall back without blocking either flow.

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

**Manual fallback**

If the AI is unavailable, Kite shows the failure and keeps working: `kt land` asks for one manual commit message (leaving it blank aborts without changing history), and `kt pr` builds a deterministic draft from the template and the branch's commits.

The failure it shows is the endpoint's own message — a rejected key, an unknown model, a schema the endpoint will not accept — so a misconfigured setup is diagnosable rather than just "the AI never works". Requests that cannot succeed on a retry are not retried.

## Why Kite Is Safe

Kite keeps the risky parts explicit:

- **Clean-worktree landing by default:** `kt land` refuses to run with staged or unstaged WIP, so scratch files do not get swept into a landed commit by surprise.
- **Dirty worktree override:** `kt land --allow-dirty` temporarily stashes uncommitted changes, lands saved commits, then restores those changes.
- **Preview before rewrite:** Kite shows the proposed commit plan before it rewrites history.
- **Rollback marker:** Every successful land records the previous `HEAD` at `refs/kite/pre_land`, along with the branch it belongs to, so `kt undo` can only ever rewind that branch.
- **No clobbering other people:** `kt go` adopts a branch that already exists on the remote instead of forking over it, and `kt publish` refuses to silently discard remote commits that are not the saves you just landed.
- **Failure leaves no mess:** landing creates no branch and never writes to your working tree, so a land that fails or is interrupted puts you back on your branch with nothing staged and nothing to clean up.
- **No dropped changes:** If the AI misses hunks, each one joins the commit already touching its file, or lands in a `chore: unclassified updates` commit — never silently omitted.
- **Exact-tree verification:** A hunk-level plan is replayed in a temporary index before any rewrite and must reproduce the pre-land tree exactly, otherwise Kite lands whole files instead.
- **Explicit publish:** Landing is local by default. Publishing remains a separate step unless you opt into `--push`.
- **Preview before PR:** `kt pr` shows the full title and body and asks for confirmation before anything reaches GitHub.

---

*Built for developers who want cheap quicksaves and cleaner review history.*
