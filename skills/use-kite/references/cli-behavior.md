# Kite CLI behavior

## Commands

### `kt go <name>`

- This command is optional. It creates and checks out a new branch for a fresh flow, or switches to the named local branch when it already exists.
- Prints `Switched to <name>` for an existing branch and `Created <name> from <base>` for a new one, so the verb tells you which happened.
- `kt`, `kt land`, and `kt publish` all operate on the current branch whether or not `kt go` was used.
- Prefer `origin/HEAD` when it exists.
- Otherwise fall back to `main`, `master`, or the current branch.
- Fetch `origin` before creating a branch. If the named branch exists there, check it out with tracking.
- Stop on a fetch failure. New branches start from the remote default branch when available, otherwise its local branch, without tracking the default branch.

### `kt`

- Run `git status --porcelain`.
- If the worktree is clean, print how many saves are ready to land (or "nothing to save") and exit without creating a commit.
- If the index already contains staged changes, create a quicksave from only that staged selection.
- Otherwise run `git add -A` and create a quicksave commit with message `[kite] save HH:MM:SS`.
- Print how many files were saved.
- Report uncommitted submodule contents that cannot be saved from the parent repository, even if Git configuration normally hides them.
- The normal recommended workflow is still to let Kite quicksave everything; staged-only quicksaves are an explicit override.
- Disable all commit hooks with a command-local `core.hooksPath` override.

### `kt land [--push|-p] [--yes] [--allow-dirty] [--tag <tag>] [--no-verify]`

- Require an existing `HEAD` commit. If the repo has no commits yet, Kite prints a warning and exits.
- By default, require a clean working tree. If the user still has WIP changes, they should `kt` them first or stash them.
- `--allow-dirty` temporarily stashes local changes before landing and restores them afterward so only contiguous `[kite] save` commits are rewritten.
- Retain the named stash backup after restoring it. Never delete a numbered stash automatically: another worktree could change the stack before deletion.
- Persist that stash's identity before creating it. After interruption, `kt undo` in the originating worktree restores its staged, unstaged, and untracked changes, including interruptions during AI planning.
- Operate only on contiguous `[kite] save` commits at the top of history.
- If saves cancel out, remove them without AI. For an entirely empty root history, replace them with one empty initial commit. Both remain undoable.
- Build the synthesis prompt from:
  - the diff introduced by those saves, plus the list of changed files the AI assigns to commits (every file lands whole in exactly one commit)
  - recent non-Kite commit messages from the current repo as style examples
- Package repository evidence in JSON. Prefer minimal coherent groups, keeping a feature with its tests, documentation, and dependency changes. Preserve repository conventions over default message style.
- Synthesize with the OpenAI Responses API using the configured base URL, model, API key, and `KITE_OPENAI_TIMEOUT_SECS` or default 120 seconds; if it is unavailable, a manual fallback asks for one commit message before rewriting history.
- Stage each landed commit as whole files, so hooks, linters, and formatters only ever see complete files.
- Show the proposed grouped commit plan, listing the files under each commit, before rewriting anything; `--yes` skips only the confirmation prompt.
- Record the pre-land `HEAD` at `refs/kite/pre_land`, and store the complete transaction phase, target, owner, and keepalive in one atomic compare-and-swap marker.
- Build commits on one exact, transaction-owned temporary branch for hook compatibility, then delete only that recorded ref with its expected commit id.
- Run the repository's commit hooks by default; `--no-verify` disables all commit hooks. Publishing still runs pre-push hooks.
- Verify that the final committed tree matches the saves. Hook changes cause rollback, preserving edits in the working tree for a new save and retry.
- Refuse to rewrite if HEAD or working files changed while planning.
- `--tag <tag>` appends ` [<tag>]` to every landed commit title, skipping titles that already carry it.
- If the process is interrupted mid-land, block further commands in that worktree until an explicit `kt undo` restores the recorded target and saves. Never infer ownership from detached `HEAD` alone.
- Work on a detached `HEAD` too: leave the landed commits under `HEAD` itself and move no branch. `--push` needs a branch, so it is refused up front when `HEAD` is detached.
- Refuse history-changing Kite commands while Git has a merge, rebase, cherry-pick, revert, bisect, `git am`, or sequencer operation in progress.
- Rewrite history locally by default.
- If `--push` is passed, publish immediately after a successful local land.
- If AI misses files, they land in a final `chore: unclassified updates` commit rather than being dropped.

### `kt publish` (alias: `kt push`)

- If no remote exists, print a note and exit successfully.
- Require a branch: `git push` has to be told which remote ref to write, so a detached `HEAD` is refused with the commit it is on and a `git switch -c <name>` hint.
- Push the current branch with `--set-upstream origin <branch>`. Fast-forward pushes are ordinary pushes; rewrites use a lease pinned to the reviewed remote commit. Remote non-save commits require confirmation. Never rebase onto stale saves first.
- A rejected lease (someone else pushed) is reported as an error for the user to reconcile manually.

### `kt pr [--draft] [--base <branch>] [--yes]`

- Requires the GitHub CLI (`gh`) to be installed and authenticated (checked offline via `gh auth token`), and a remote to exist.
- Refuses to run on the base branch, on a detached `HEAD`, or with unlanded `[kite] save` commits anywhere in the branch's changes. Top saves can be landed normally; buried saves need consolidation first.
- Target `origin` explicitly, excluding fork PRs with the same branch name. Fetch and push URLs must identify the same repository; equivalent SSH and HTTPS URLs are accepted.
- If an open pull request already exists for the branch, pushes any new commits, asks the AI whether the body still reflects the branch, and offers a refreshed body (`gh pr edit`) after preview and confirmation; if it still fits, prints "nothing to update". Without AI the existing body is left untouched. Merged or closed PRs do not block a new one.
- Fetches and validates the base branch, then publishes using the normal publish checks. Lookup failures stop before publication.
- Stop if the branch or HEAD changes during lookup, drafting, or review. Draft from captured commit ids so concurrent changes cannot silently replace the reviewed input.
- Gathers context for the draft:
  - the commits and diff between the base branch and `HEAD`
  - the repository's pull request template (checked case-insensitively in the root, `.github/`, `docs/`, and `.github/PULL_REQUEST_TEMPLATE/`); the AI fills it in and removes sections that don't apply rather than leaving them empty or writing N/A
  - dedicated PR writing skills from `.claude/skills`, `.codex/skills`, `.agents/skills`, and `skills` in the repo, plus `~/.claude/skills`, `~/.codex/skills`, and `~/.agents/skills`; project writing guidance precedes user guidance, the template, and title examples
  - recent merged pull request titles as style examples
- Drafts the title and body with the same OpenAI Responses API as `kt land`; without AI a new PR uses a clean generic `## Summary` section populated from branch commit subjects and never copies an unfilled repository template.
- Read at most three skills, 4 KB each, and a 6 KB template. Do not follow referenced files. Exclude incidental PR mentions in operational skills such as `use-kite`; skill instructions control writing only, never command execution. Do not claim tests passed merely because tests were added.
- Previews the draft and asks for confirmation before running `gh pr create` (skip with `--yes`).
- The branch is already published when the preview appears. Declining prevents PR creation or editing, not the earlier push.
- `--draft` creates a draft pull request; `--base` overrides the detected default branch.

### `kt undo`

- Reverse the most recent thing Kite did: the quicksave on top of history if there is one, otherwise the last land.
- Undoing a quicksave is a mixed reset, so it needs no clean tree and keeps edits made since.
- Undoing the first save returns the branch to its unborn state and empties the index while preserving files. A detached first commit cannot be undone this way.
- Recover interrupted landing and any pending temporary stash before undoing another save or completed land.
- Undoing a land requires a clean working tree, restores the pre-land saves, then clears its rollback marker. Interrupted undo preserves later working-tree edits or refuses if they conflict.
- Only undo a land where it happened — a branch, or a detached `HEAD` in the same linked worktree. Anywhere else it refuses and says where to go.
- If `origin` still points to the exact landed commit, restore the pre-land saves remotely using an explicit lease. Leave newer remote work and detached lands alone.

## OpenAI environment variables

- Base URL: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`, `AI_GATEWAY_API_KEY`
- Timeout: `KITE_OPENAI_TIMEOUT_SECS`
- Default base URL: `https://api.openai.com/v1`
- Default model: `gpt-5.4-mini`
- Default timeout: 120 seconds
- Kite normalizes base URLs before calling `/responses`:
  - strips a trailing `/responses`
  - strips a trailing `/chat/completions`
  - appends `/v1` when it is missing

## Practical preflight checks

- `git status --short --branch`
- `git log --oneline -n 12`
- `command -v kt`
- `git remote -v`
- `command -v gh` (before `kt pr`)
