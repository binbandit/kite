# Kite CLI behavior

## Commands

### `kt go <name>`

- This command is optional. It creates and checks out a new branch for a fresh flow, or switches to the named local branch when it already exists.
- Prints `Switched to <name>` for an existing branch and `Created <name> from <base>` for a new one, so the verb tells you which happened.
- `kt`, `kt land`, and `kt publish` all operate on the current branch whether or not `kt go` was used.
- Use the base remote's recorded `HEAD` (see Forks below). When a base remote other than `origin` has none, ask it with `git ls-remote --symref <remote> HEAD` rather than guessing from the fork.
- Otherwise fall back to `main`, `master`, or the current branch.
- Fetch `origin`, and the base remote when it differs, before creating a branch. If the named branch exists on `origin`, check it out with tracking.
- Stop on a fetch failure. New branches start from the base remote's default branch when available, otherwise its local branch, without tracking the default branch.

### `kt`

- Outside a Git repository, show help without creating anything. Inside one, inspect status from the repository root, including all untracked files and submodule changes.
- If the worktree is clean, print how many saves are ready to land (or "nothing to save") and exit without creating a commit.
- If the index already contains staged changes, create a quicksave from only that staged selection.
- Otherwise run `git add -A` and create a quicksave commit with message `[kite] save HH:MM:SS`.
- Print how many files were saved.
- Report uncommitted submodule contents that cannot be saved from the parent repository, even if Git configuration normally hides them.
- The normal recommended workflow is still to let Kite quicksave everything; staged-only quicksaves are an explicit override.
- Disable all commit hooks with a command-local `core.hooksPath` override.

### `kt land [--push|-p] [--yes|-y] [--allow-dirty] [--tag|-t <tag>] [--no-verify] [--granularity|-g coarse|fine|per-save]`

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
- Synthesize with the OpenAI Responses API using the configured base URL, model, API key, and `KITE_OPENAI_TIMEOUT_SECS` or default 120 seconds; if it is unavailable, interactive mode asks for one manual commit message before rewriting history. With `--yes`, fail without rewriting and instruct the user to rerun interactively.
- Stage each landed commit as whole files, so hooks, linters, and formatters only ever see complete files.
- Show the proposed grouped commit plan, listing the files under each commit, before rewriting anything; `--yes` accepts the plan automatically and fails instead of prompting for a manual message when AI is unavailable. It does not bypass hooks or remote-history replacement confirmation.
- Record the pre-land `HEAD` at `refs/kite/pre_land`, and store the complete transaction phase, target, owner, and keepalive in one atomic compare-and-swap marker.
- Build commits on one exact, transaction-owned temporary branch for hook compatibility, then delete only that recorded ref with its expected commit id.
- Run the repository's commit hooks by default; `--no-verify` disables all commit hooks. Publishing still runs pre-push hooks.
- Accept successful hook changes committed to the current group's files and verify every other file against the saves. Report included hook changes. Reject staging outside the planned group or staged leftovers after commit; failed hooks still roll back with edits preserved.
- Remove redundant empty commits when formatting cancels the group's changes. Keep an empty initial commit when there is no parent.
- Persist the validated landed commit alongside pending stashed work before returning to the original checkout, so `--allow-dirty` can restore work onto the formatted result after an interruption. Real stash conflicts retain the backup and require resolution.
- Refuse to rewrite if HEAD or working files changed while planning.
- `--tag <tag>` appends ` [<tag>]` to every landed commit title, skipping titles that already carry it.
- If the process is interrupted mid-land, block further commands in that worktree until an explicit `kt undo` restores the recorded target and saves. Never infer ownership from detached `HEAD` alone.
- Work on a detached `HEAD` too: leave the landed commits under `HEAD` itself and move no branch. `--push` needs a branch, so it is refused up front when `HEAD` is detached.
- Refuse history-changing Kite commands while Git has a merge, rebase, cherry-pick, revert, bisect, `git am`, or sequencer operation in progress.
- Rewrite history locally by default.
- If `--push` is passed, publish immediately after a successful local land.
- If AI misses files, they land in a final `chore: unclassified updates` commit rather than being dropped.
- `--granularity` chooses how finely saves become commits; the final tree is the saved tip at every setting, apart from committed hook changes.
  - `coarse` (default) plans all saved changes at once, as described above.
  - `fine` cuts the saves into windows of consecutive saves: at a pause of 30 minutes or more, or once a window reaches 12 saves, 5,000 changed lines, or 300 changed files. A single larger save is a window of its own. Each window is planned on its own and lands as one or more commits, in order, ending on the exact tree of its last save. A window whose saves cancel out lands nothing.
  - `per-save` keeps every save as its own commit with its exact tree and writes it a message. Up to eight consecutive saves share one request, splitting one diff budget.
  - Both send bounded requests, at most `KITE_OPENAI_CONCURRENCY` (default 4) at once, each with the normal timeout, retries, and file-coverage repair; progress shows while they run. Any failed request stops the land with no history changed and no manual-message fallback, with or without `--yes`. The prompt tells the model which slice of the history it is planning.
  - Both build commits in a private linked worktree inside an owner-only (`0700`) `kite-land-<transaction id>` folder under the system temp directory, attached to the transaction branch. Each commit stages its files' exact saved entries, including submodule pointers, which never need the submodule checked out. Hooks see each commit's files as saved; the user's worktree, index, and untracked or ignored files are not touched until the end, when a two-way `read-tree` updates only files hooks changed, refusing to overwrite one edited meanwhile. Committed hook edits carry into later windows that leave the file alone; unstaged hook edits are discarded with the private worktree. Each window is verified against its save, excusing only that window's committed hook changes.
  - Hooks there cannot use untracked files from the user's worktree, such as installed dependencies, and may reject an earlier save that a later one fixed; the error names the saves and suggests `--no-verify` or a coarser granularity. The private worktree is removed when the land ends, and by `kt undo` after an interruption.
  - The preview groups commits under the saves they come from and shows each commit's subject and file count, not its files or body.

### `kt publish` (alias: `kt push`)

- If no `origin` remote exists, print a note and exit successfully.
- Require a branch: `git push` has to be told which remote ref to write, so a detached `HEAD` is refused with the commit it is on and a `git switch -c <name>` hint.
- Push the current branch with `--set-upstream origin <branch>`. Fast-forward pushes are ordinary pushes; rewrites use a lease pinned to the reviewed remote commit. Remote non-save commits require confirmation. Never rebase onto stale saves first.
- A rejected lease (someone else pushed) is reported as an error for the user to reconcile manually.

### `kt pr [--draft] [--base <branch>] [--yes|-y]`

- Requires the GitHub CLI (`gh`) to be installed and authenticated, and an `origin` remote to exist. Repository and PR lookups through `gh` validate access.
- Refuses to run on the base branch, on a detached `HEAD`, or with unlanded `[kite] save` commits anywhere in the branch's changes. Top saves can be landed normally; buried saves need consolidation first.
- Target the base remote's repository explicitly. In a fork, publish to `origin`, fetch the base branch from the parent, and create the PR there with `--head <origin-owner>:<branch>`. Reuse an existing PR only when its head repository is `origin`. Fetch and push URLs must identify the same repository; equivalent SSH and HTTPS URLs are accepted.
- If `origin` is a GitHub fork with no base remote configured, stop before publishing and suggest adding the parent as `upstream` or running `gh repo set-default origin`.
- If an open pull request already exists for the branch, pushes any new commits, asks the AI whether the title and body still reflect the branch, and offers refreshed text (`gh pr edit`) after preview and confirmation; if it still fits, prints "Pull request already reflects the branch". Without AI the existing title and body are left untouched. Merged or closed PRs do not block a new one.
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
- `--draft` creates a draft pull request and does not convert an existing PR. The base defaults to the existing PR's base when one exists, otherwise the repository default. `--base` overrides the default for new PRs; a mismatch with an existing PR's base is an error. Multiple matching open PRs also stop the command.

### `kt ship [--draft] [--base <branch>] [--tag|-t <tag>] [--no-verify] [--granularity|-g coarse|fine|per-save]`

- Combine quicksave, land, publish, and PR creation or refresh. Equivalent workflow to `kt && kt land -yp && kt pr -y`, but validate the PR destination before saving and publish once through the PR step after checking the landed history.
- Automatically accept land and PR previews; there is no `--yes` flag. Keep the normal hooks, explicit push leases, and confirmation before discarding remote non-save commits.
- Require an existing commit, `origin`, an attached branch other than the PR base, and working GitHub access. Reuse `kt pr` destination, fork, existing-PR, and base validation.
- Use the ordinary quicksave rules. An existing staged subset is respected; leftover changes block landing. There is no `--allow-dirty` option.
- Land with `--yes`. Forward `--tag`, `--no-verify`, and `--granularity` to landing, and `--draft` and `--base` to PR handling. `--no-verify` does not skip pre-push hooks.
- Stop at the first error; completed steps remain. AI planning errors leave the quicksave intact and require interactive `kt land` for manual messages. Failed publication leaves local commits; failed PR writes may leave a published branch. After fixing the cause, rerun `kt ship` or the remaining command.
- Clean saves and lands with no pending saves are no-ops, so shipping can resume after a partial success or refresh an existing PR. Buried saves or no commits relative to the base stop before publishing.
- Never merge the PR. `kt undo` handles the saved or landed history as usual and does not remove the PR.

### `kt undo`

- Reverse the most recent thing Kite did: the quicksave on top of history if there is one, otherwise the last land.
- Undoing a quicksave is a mixed reset, so it needs no clean tree and keeps edits made since.
- Undoing the first save returns the branch to its unborn state and empties the index while preserving files. A detached first commit cannot be undone this way.
- Recover interrupted landing and any pending temporary stash before undoing another save or completed land.
- Undoing a land requires a clean working tree, restores the pre-land saves, then clears its rollback marker. Interrupted undo preserves later working-tree edits or refuses if they conflict.
- Only undo a land where it happened — a branch, or a detached `HEAD` in the same linked worktree. Anywhere else it refuses and says where to go.
- If `origin` still points to the exact landed commit, restore the pre-land saves remotely using an explicit lease. Leave newer remote work and detached lands alone.

## Forks

- Kite always publishes to `origin`.
- The base remote decides where new branches start and where pull requests go: the remote marked by `gh repo set-default` (`remote.<name>.gh-resolved = base`), otherwise `upstream`, otherwise `origin`.
- When `gh-resolved` names a repository (`owner/name`) instead, use the remote whose URL points to it. If none does, stop and suggest `git remote add upstream <url>`.
- `kt publish` and `kt undo` never touch the base remote.

## OpenAI environment variables

- Base URL: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`, `AI_GATEWAY_API_KEY`
- Reasoning effort: `KITE_OPENAI_REASONING_EFFORT`
- Timeout: `KITE_OPENAI_TIMEOUT_SECS`
- Concurrency for `fine` and `per-save` lands: `KITE_OPENAI_CONCURRENCY`
- Default base URL: `https://api.openai.com/v1`
- Default model: `gpt-5.4`
- Default reasoning effort: `low`, dropped and retried when a model or gateway rejects it
- Default timeout: 120 seconds per request
- Default concurrency: 4. Throttled or failed requests wait before retrying: the provider's `retry-after-ms` or `retry-after`, up to a minute, otherwise one and then two seconds
- Kite normalizes base URLs before calling `/responses`:
  - strips a trailing `/responses`
  - strips a trailing `/chat/completions`
  - appends `/v1` when it is missing

## Practical preflight checks

- `git status --short --branch`
- `git log --oneline -n 12`
- `command -v kt`
- `git remote -v`
- `command -v gh` (before `kt pr` or `kt ship`)
