# Kite CLI behavior

## Commands

### `kt go <name>`

- This command is optional. It creates and checks out a new branch for a fresh flow, or switches to the named local branch when it already exists.
- When switching to an existing branch, print a note that no new branch was created.
- `kt`, `kt land`, and `kt publish` all operate on the current branch whether or not `kt go` was used.
- Prefer `origin/HEAD` when it exists.
- Otherwise fall back to `main`, `master`, or the current branch.
- If a remote exists, fetch `origin/<default-branch>` and create the new branch from it when possible.
- If that remote checkout fails, create the branch from the local default branch.

### `kt`

- Run `git status --porcelain`.
- If the worktree is clean, exit without creating a commit.
- If the index already contains staged changes, create a quicksave from only that staged selection.
- Otherwise run `git add -A` and create a quicksave commit with message `[kite] save HH:MM:SS`.
- The normal recommended workflow is still to let Kite quicksave everything; staged-only quicksaves are an explicit override.
- Pass `--no-verify`, so Git hooks do not run for quicksaves.

### `kt land [--push] [--yes] [--allow-dirty]`

- Require an existing `HEAD` commit. If the repo has no commits yet, Kite prints a warning and exits.
- By default, require a clean working tree. If the user still has WIP changes, they should `kt` them first or stash them.
- `--allow-dirty` temporarily stashes local changes before landing and restores them afterward so only contiguous `[kite] save` commits are rewritten.
- Operate only on contiguous `[kite] save` commits at the top of history.
- Build the synthesis prompt from:
  - the diff introduced by those saves
  - recent non-Kite commit messages from the current repo as style examples
- Try providers in this order:
  1. Local Ollama at `http://localhost:11434/api/chat` with `KITE_LOCAL_MODEL` or default `llama3`
  2. OpenAI Responses API using the configured base URL, model, and API key
  3. Manual fallback that asks for one commit message before rewriting history
- Show the proposed grouped commit plan before rewriting anything, unless `--yes` is passed.
- Record the pre-land `HEAD` at `refs/kite/pre_land`.
- If rewriting fails mid-land, keep the in-progress state on a `kite-recovery-*` branch so partial commits or staged changes are preserved.
- Rewrite history locally by default.
- If `--push` is passed, publish immediately after a successful local land.
- If AI misses files, add a final `chore: unclassified updates` commit for the leftovers.

### `kt publish`

- If no remote exists, print a note and exit successfully.
- Otherwise try `git pull --rebase origin <branch>`.
- Then push the current branch with `--set-upstream origin <branch> --force-with-lease`.

### `kt undo`

- Require a clean working tree.
- Reset hard to `refs/kite/pre_land`, then delete that ref.
- If a remote exists, force-push the current branch with `--force-with-lease`.

## OpenAI environment variables

- Base URL: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`
- Default base URL: `https://api.openai.com/v1`
- Default model: `gpt-5.4-mini`
- Kite normalizes base URLs before calling `/responses`:
  - strips a trailing `/responses`
  - strips a trailing `/chat/completions`
  - appends `/v1` when it is missing

## Practical preflight checks

- `git status --short --branch`
- `git log --oneline -n 12`
- `command -v kt`
- `git remote -v`
