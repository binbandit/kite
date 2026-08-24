# Kite CLI behavior

## Commands

### `kt go <name>`

- This command is optional. It creates and checks out a new branch for a fresh flow, or switches to the named local branch when it already exists.
- Prints `Switched to <name>` for an existing branch and `Created <name> from <base>` for a new one, so the verb tells you which happened.
- `kt`, `kt land`, and `kt publish` all operate on the current branch whether or not `kt go` was used.
- Prefer `origin/HEAD` when it exists.
- Otherwise fall back to `main`, `master`, or the current branch.
- If a remote exists, fetch `origin/<default-branch>` and create the new branch from it when possible.
- If that remote checkout fails, create the branch from the local default branch.

### `kt`

- Run `git status --porcelain`.
- If the worktree is clean, print how many saves are ready to land (or "nothing to save") and exit without creating a commit.
- If the index already contains staged changes, create a quicksave from only that staged selection.
- Otherwise run `git add -A` and create a quicksave commit with message `[kite] save HH:MM:SS`.
- Print how many files were saved.
- The normal recommended workflow is still to let Kite quicksave everything; staged-only quicksaves are an explicit override.
- Pass `--no-verify`, so Git hooks do not run for quicksaves.

### `kt land [--push|-p] [--yes] [--allow-dirty] [--tag <tag>] [--no-verify]`

- Require an existing `HEAD` commit. If the repo has no commits yet, Kite prints a warning and exits.
- By default, require a clean working tree. If the user still has WIP changes, they should `kt` them first or stash them.
- `--allow-dirty` temporarily stashes local changes before landing and restores them afterward so only contiguous `[kite] save` commits are rewritten.
- Operate only on contiguous `[kite] save` commits at the top of history.
- Build the synthesis prompt from:
  - the diff introduced by those saves, plus the list of changed files the AI assigns to commits (every file lands whole in exactly one commit)
  - recent non-Kite commit messages from the current repo as style examples
- Synthesize with the OpenAI Responses API using the configured base URL, model, API key, and `KITE_OPENAI_TIMEOUT_SECS` or default 120 seconds; if it is unavailable, a manual fallback asks for one commit message before rewriting history.
- Stage each landed commit as whole files, so hooks, linters, and formatters only ever see complete files.
- Show the proposed grouped commit plan, listing the files under each commit, before rewriting anything; `--yes` skips only the confirmation prompt.
- Record the pre-land `HEAD` at `refs/kite/pre_land`, and store the complete transaction phase, target, owner, and keepalive in one atomic compare-and-swap marker.
- Build commits on one exact, transaction-owned temporary branch for hook compatibility, then delete only that recorded ref with its expected commit id.
- Run the repository's commit hooks by default; `--no-verify` commits with `git commit --no-verify`, so the `pre-commit` and `commit-msg` hooks do not run. Landing never bypasses the `pre-push` hook — `--push` publishes normally.
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
- Push the current branch with `--set-upstream origin <branch> --force-with-lease` — no `pull --rebase` first, so a land never gets rebased onto the remote's stale saves.
- A rejected lease (someone else pushed) is reported as an error for the user to reconcile manually.

### `kt pr [--draft] [--base <branch>] [--yes]`

- Requires the GitHub CLI (`gh`) to be installed and authenticated (checked offline via `gh auth token`), and a remote to exist.
- Refuses to run on the base branch, on a detached `HEAD`, or with unlanded `[kite] save` commits (run `kt land` first).
- If an open pull request already exists for the branch, pushes any new commits, asks the AI whether the body still reflects the branch, and offers a refreshed body (`gh pr edit`) after preview and confirmation; if it still fits, prints "nothing to update". Without AI the existing body is left untouched. Merged or closed PRs do not block a new one.
- Fetches `origin/<branch>` and publishes when the remote is missing the branch or out of date.
- Gathers context for the draft:
  - the commits and diff between the base branch and `HEAD`
  - the repository's pull request template (checked case-insensitively in the root, `.github/`, `docs/`, and `.github/PULL_REQUEST_TEMPLATE/`); the AI fills it in and removes sections that don't apply rather than leaving them empty or writing N/A
  - PR-related agent skills (`SKILL.md` files whose name or frontmatter mentions pull requests) from `.claude/skills`, `.agents/skills`, and `skills` in the repo, plus `~/.claude/skills`, `~/.codex/skills`, and `~/.agents/skills` — treated as the user's own instructions and given precedence over the default drafting rules
  - recent merged pull request titles as style examples
- Drafts the title and body with the same OpenAI Responses API as `kt land`; without AI a new PR uses a clean generic `## Summary` section populated from branch commit subjects and never copies an unfilled repository template.
- Previews the draft and asks for confirmation before running `gh pr create` (skip with `--yes`).
- `--draft` creates a draft pull request; `--base` overrides the detected default branch.

### `kt undo`

- Reverse the most recent thing Kite did: the quicksave on top of history if there is one, otherwise the last land.
- Undoing a quicksave is a mixed reset, so it needs no clean tree and keeps edits made since.
- Undoing a land requires a clean working tree, resets hard to `refs/kite/pre_land`, then deletes that ref.
- Only undo a land where it happened — a branch, or a detached `HEAD` in the same linked worktree. Anywhere else it refuses and says where to go.
- If a remote exists and the land was on a branch, force-push that branch with `--force-with-lease`. A detached land was never publishable, so the remote is left alone.

## OpenAI environment variables

- Base URL: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`
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
