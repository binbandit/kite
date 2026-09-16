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

Quicksaves intentionally disable all commit hooks to stay fast. Landed commits use normal `git commit` behavior, so hooks do run when polished history is written.

### 2. Land Saved Work Into Reviewable Commits

When your branch is full of contiguous Kite saves, run:

```bash
kt land
```

Kite analyzes the diff introduced by those saves, proposes logical commit groups, and only rewrites history after you confirm the plan.

Grouping is file-level: every file lands whole, in exactly one commit. That keeps each commit something your tooling can actually run: a pre-commit hook, linter, or formatter always sees complete files, never a half-applied one.

Kite also feeds recent non-Kite commit messages from the current repository into the prompt so landed messages follow the repo's existing style when possible. If the repo does not show a clear pattern, Kite falls back to Conventional Commit style.

The prompt prefers the fewest coherent commits. A feature stays with its tests, documentation, and dependency changes; a shared file keeps related work together.

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

Kite drives the `git` binary on your PATH and needs version 2.25 or newer. Remote commands use the remote named `origin`.

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

Kite prefers `origin/HEAD` when it exists, otherwise falls back to `main`, `master`, or the current branch. It stops if fetching `origin` fails, and new branches do not automatically track the default branch.

```bash
kt go stripe-webhooks
```

### `kt`

The zero-friction quicksave. Run this constantly while you work.

- If the worktree is clean, Kite exits without creating a commit.
- If you already staged a deliberate subset, Kite quicksaves only that staged selection.
- Otherwise Kite runs `git add -A` and snapshots tracked plus untracked changes.
- Changes inside a submodule must be saved in that submodule first. Kite reports them even when Git is configured to hide submodule changes.
- Quicksaves disable all commit hooks, including message preparation and post-commit hooks.
- Saved one you did not mean to? `kt undo` puts it straight back in your working tree.

```bash
kt
```

### `kt land`

Synthesizes contiguous Kite quicksaves into a polished local history.

- Requires an existing `HEAD` commit.
- By default, requires a clean working tree. If you still have WIP changes, run `kt` first or stash them.
- Use `--allow-dirty` to land while your worktree is dirty; `kt` temporarily stashes and restores those changes, retaining a named stash backup.
- If interrupted while those changes are stashed, run `kt undo` in the same worktree to recover them, including the staged selection. This also works if interruption happened while waiting for AI.
- Works on a detached `HEAD`: the landed commits are left under `HEAD` itself and no branch is moved. `--push` still needs a branch, and says so before anything is rewritten.
- Refuses to start during an active merge, rebase, cherry-pick, revert, bisect, `git am`, or sequencer operation; their temporary detached checkouts are not standalone worktrees.
- Only rewrites contiguous `[kite] save` commits at the top of history, following first-parent history so a merge cannot move the starting point.
- Saves that cancel each other out can be removed without AI. If they cover the repository's entire history, Kite replaces them with one empty initial commit. Both outcomes remain undoable.
- Groups whole files: every changed file lands in exactly one commit, so hooks and linters never see a partially applied file.
- Shows the proposed commit plan, with the files under each commit, before rewriting anything.
- Stores the pre-land `HEAD` in `refs/kite/pre_land` and updates the full rollback transaction atomically, so `kt undo` can restore it later without linked worktrees observing a half-written marker.
- Creates normal `git commit`s, so hooks do run during landing. Pass `--no-verify` to disable all commit hooks.
- Accepts changes that successful commit hooks stage to the current commit's planned files, so formatter differences between your editor and hooks land in one run. Kite reports the included hook changes and still verifies every other file against the saves.
- If formatting cancels a group's changes completely, Kite removes the redundant empty commit. An empty initial commit is kept when the repository has no parent commit.
- Refuses hooks that stage files outside the current group or leave staged changes after the commit was written. Failed hooks still restore your saves and preserve their edits for correction and retry.
- Stops if your checkout, commits, or working files change while the plan is being prepared or reviewed.
- Landing builds on one uniquely named temporary branch so ordinary Git hooks see a normal checkout. It records that exact ref, moves your branch with a compare-and-swap — or, if you were already detached, moves `HEAD` itself — and removes the temporary branch before returning.
- If landing fails for any reason — a rejected pre-commit hook is the usual one — Kite undoes the attempt and leaves you exactly where you started: on your branch or your detached commit, saves intact, nothing staged, no branch to clean up. Fix the problem and run `kt land` again. Files a hook rewrote are kept as unstaged changes.
- If landing is interrupted rather than failing — Ctrl-C, a crash, a closed terminal — the next `kt` command stops and asks you to run `kt undo`. Recovery is explicit because a worktree id alone cannot prove that a detached commit checked out later is still Kite's partial rewrite.
- Lands locally by default. Use `kt publish` afterward, or pass `--push` (`-p`) to publish immediately after landing.

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

Land without running the commit hooks (`--push` still runs the pre-push hook):

```bash
kt land --no-verify
```

Tag every landed commit title with a reference:

```bash
kt land --tag PROJ-123
```

Land and publish in one step:

```bash
kt land --push
```

### `kt publish` (alias: `kt push`)

Publishes the current branch after you review the rewritten local history.

- Pushes with `--set-upstream origin <current-branch>`. Uses the current remote-tracking ref when available, otherwise fetches the branch first.
- Forces only when it has to. If the remote is already an ancestor of your branch, the push is an ordinary fast-forward and nothing is forced.
- Deliberately no `git pull --rebase` first: after a land, the remote still holds the old saves, and rebasing onto them would resurrect the history you just rewrote.
- When the remote holds commits your branch does not, Kite looks at what a force would discard. Kite saves are the history you just rewrote, so those go without ceremony. Anything else is someone's work: Kite lists the commits and asks before touching them.
- If no remote exists, Kite exits without error and leaves the history local.
- Requires a branch. `git push` has to be told which remote ref to write, and a detached `HEAD` supplies no name, so Kite names the commit you are on and points you at `git switch -c <name>`.

Kite uses an explicit expected remote commit when force-pushing. A background fetch cannot silently change the commit that Kite reviewed and agreed to replace; a newer remote commit causes the push to fail safely.

```bash
kt publish
```

### `kt pr`

Opens a GitHub pull request for the current branch using the [GitHub CLI](https://cli.github.com) (`gh`). It is deliberately smart about the draft:

- Requires `gh` to be installed and authenticated, a remote to exist, and a branch to open the pull request from — a detached `HEAD` is refused the same way `kt publish` refuses it.
- Refuses to run with unlanded saves anywhere in the branch's changes. Run `kt land` for saves on top; saves buried beneath ordinary commits need to be consolidated first.
- Publishes the branch automatically before showing the draft. Declining the draft prevents PR creation or editing, but does not undo that push.
- Targets the repository configured as `origin` and excludes same-name branches from forks when looking for an existing PR. Fetch and push URLs may use different protocols, but must identify the same repository.
- If a pull request is already open for the branch, Kite pushes any new commits, checks whether the body still reflects the branch, and offers a refreshed body when it doesn't — preserving the existing structure and any human-written notes. Without AI, the existing body is never touched.
- With AI available, finds the repository's pull request template in `.github/`, the repo root, `docs/`, and `.github/PULL_REQUEST_TEMPLATE/`. The prompt asks it to fill useful sections and omit empty headings, `N/A`, and leftover boilerplate; review the preview before accepting it.
- Discovers dedicated PR writing skills in the repository's `.claude/skills`, `.codex/skills`, `.agents/skills`, and `skills`, followed by the user's `~/.claude/skills`, `~/.codex/skills`, and `~/.agents/skills`. Project writing guidance takes precedence over user guidance, then the template and title examples. Operational skills such as `use-kite` and PR monitoring are excluded.
- Reads up to three writing skills, with at most 4 KB per skill and 6 KB for the template. It reads each `SKILL.md` directly without following referenced files. Skills influence prose only; their instructions to run commands or change Git state do not control Kite.
- Fetches and validates the base branch before publishing. A failed pull request lookup stops the command instead of being treated as permission to create a duplicate.
- Uses recent merged pull request titles from the repository as style examples for the new title.
- Drafts the title and body with the same AI as `kt land`. Without AI, a new PR uses a clean generic `## Summary` section populated from the branch's commit subjects and never copies an unfilled repository template.
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
- Clears the saved selection from staging, including when undoing the repository's first commit.
- Never touches the working tree, so edits you made after the save survive and the tree does not need to be clean.
- Entirely local — nothing is pushed.
- The one case it cannot handle: a save that is the repository's very first commit while `HEAD` is detached. There is no unborn detached state to return to, so Kite says so instead of deleting the commit `HEAD` points at.

**Undoing a land**

- Requires a clean working tree.
- Only undoes where the land happened. Landing records that place — a branch, or a detached `HEAD` — so running `kt undo` from somewhere else refuses and tells you where to go, naming the commit to `git switch --detach` back onto when the land was detached. It cannot reset an unrelated branch to unrelated history.
- Asks first if `HEAD` has moved since the land, since those newer commits would be discarded.
- Restores the pre-land saves. If `origin` still points to the exact landed commit, Kite restores those saves remotely too. Newer remote work is left untouched, even after a background fetch. Detached lands are restored locally.
- Deletes the rollback marker after a successful undo so you do not accidentally replay it twice.

```bash
kt undo
```

### Detached `HEAD`

Kite works on a detached `HEAD` — for example, in a linked worktree, while reviewing a colleague's commit, or while sitting on a tag — without asking you to invent a branch first.

- `kt` quicksaves as usual. The snapshots stack on `HEAD` exactly as they would on a branch.
- `kt land` rewrites those saves and leaves `HEAD` detached on the landed commits. No persistent branch is created, and the branch you stepped off is not moved.
- `kt undo` rewinds the detached `HEAD`, and refuses if you have since moved somewhere else — a land recorded on a branch will not be undone from a detached `HEAD`, and vice versa. The refusal names the commit to `git switch --detach` back onto.
- An interrupted land is detected before another command can build on its partial history. Run `kt undo` in the originating worktree to put `HEAD` back on the commit it started from with every save intact.
- Rollback and interrupted-land recovery are tied to the originating worktree, so a Kite command in another detached worktree cannot move or undo it.
- `kt publish`, `kt pr`, and `kt land --push` need a branch, because `git push` has to be told which remote ref to write and a detached `HEAD` supplies no name. They refuse before anything is rewritten and tell you the commit you are on, so `git switch -c <name>` is a one-liner away.

Nothing about the detached path is a special mode: the same rollback marker, plan preview, and exact-tree verification apply.

## Configuration & AI

Both `kt land` and `kt pr` use one AI, reached through the OpenAI Responses API, and fall back without blocking either flow.

- Base URL env precedence: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model env precedence: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key env precedence: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`, `AI_GATEWAY_API_KEY`
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

If the AI is unavailable, Kite shows the failure and keeps working: `kt land` asks for one manual commit message (leaving it blank aborts without changing history), and a new `kt pr` uses a clean generic `## Summary` section populated from the branch's commit subjects without copying an unfilled repository template.

The failure it shows is the endpoint's own message — a rejected key, an unknown model, a schema the endpoint will not accept — so a misconfigured setup is diagnosable rather than just "the AI never works". Requests that cannot succeed on a retry are not retried.

Prompts separate instructions from repository evidence using JSON context. They ask for supported changes only: adding a test does not justify claiming the test passed. Git operations and file coverage remain controlled by Kite. See [Reviewing AI output](docs/ai-output-evaluation.md) for cases and a scoring guide when changing prompts or models.

## Why Kite Is Safe

Kite keeps the risky parts explicit:

- **Clean-worktree landing by default:** `kt land` refuses to run with staged or unstaged WIP, so scratch files do not get swept into a landed commit by surprise.
- **Dirty worktree override:** `kt land --allow-dirty` temporarily stashes uncommitted changes, lands saved commits, then restores that exact stash, including staging. It retains the named stash backup because deleting a numbered stash could race with another worktree's stash operation.
- **Preview before rewrite:** Kite shows the proposed commit plan before it rewrites history.
- **Atomic rollback marker:** Every land records the previous `HEAD`, target, owner, and phase in one compare-and-swap ref transaction, with `refs/kite/pre_land` retained as the recovery pointer. Linked worktrees cannot interleave marker fields, and `kt undo` can only rewind the recorded place.
- **No clobbering other people:** `kt go` adopts a branch that already exists on the remote instead of forking over it, and `kt publish` refuses to silently discard remote commits that are not the saves you just landed.
- **Failure leaves no mess:** landing deletes its exact temporary branch and never overwrites your working files, so a normal failure puts you back where you were — branch or detached commit — with nothing staged and nothing to clean up. If the process itself is interrupted, the originating worktree is blocked until `kt undo` performs the same recovery explicitly.
- **Concurrent-command guard:** one Kite command at a time may mutate a worktree, and branch moves use expected old commit ids. A branch advanced or checked out elsewhere is left untouched.
- **No dropped changes:** If the AI misses a file, it still lands, in a `chore: unclassified updates` commit, rather than being silently omitted.
- **Explicit publish:** Landing is local by default. Publishing remains a separate step unless you opt into `--push`.
- **Preview before PR creation:** `kt pr` publishes the branch, then shows the full title and body for confirmation before creating or editing the PR.

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) for local checks, a map of the code, and the behavior changes must preserve. The normal test suite needs no AI account or GitHub credentials.

---

*Built for developers who want cheap quicksaves and cleaner review history.*
