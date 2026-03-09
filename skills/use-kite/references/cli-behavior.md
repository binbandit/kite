# Kite CLI behavior

## Commands

### `kt go <name>`

- Detect the default branch by checking for `main`; otherwise fall back to `master`.
- If a remote exists, fetch `origin/<default-branch>` and create the branch from it when possible.
- If that remote checkout fails, create the branch from the local default branch.

### `kt`

- Run `git status --porcelain`.
- If the worktree is clean, exit without creating a commit.
- If the index already contains staged changes, create a quicksave from only that staged selection.
- Otherwise run `git add -A` and create a quicksave commit with message `[kite] save HH:MM:SS`.
- The normal recommended workflow is still to let Kite quicksave everything; staged-only quicksaves are an explicit override.
- Pass `--no-verify`, so Git hooks do not run for quicksaves.

### `kt land`

- Require an existing `HEAD` commit. If the repo has no commits yet, Kite prints a warning and exits.
- Rewind only contiguous `[kite] save` commits at the top of history.
- Stage all changes, inspect the cached diff, then unstage so Kite can create surgical commits.
- Try providers in this order:
  1. Local Ollama at `http://localhost:11434/api/chat` with `KITE_LOCAL_MODEL` or default `llama3`
  2. OpenAI Responses API using the configured base URL, model, and API key
  3. Manual fallback that asks for one Conventional Commit message and squashes everything into it
- Create grouped commits from the provider output. Leftover files become `chore: unclassified updates`.
- Run normal `git commit`, so hooks do run during landing.
- If a remote exists, push the current branch with `--set-upstream origin <branch> --force-with-lease`.

### `kt undo`

- Require a clean working tree.
- Reset hard to `refs/kite/pre_land`, then delete that ref.
- If a remote exists, force-push the current branch.
- Current caveat: `src/main.rs` checks for `refs/kite/pre_land`, but the same file does not obviously write that ref during `kt land`. Verify the implementation before relying on `kt undo` as a recovery path.

## OpenAI environment variables

- Base URL: `KITE_OPENAI_URL`, `KITE_OPENAI_BASE_URL`, `OPENAI_URL`, `OPENAI_BASE_URL`
- Model: `KITE_OPENAI_MODEL`, `OPENAI_MODEL`
- API key: `KITE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `KITE_API_KEY`, `OPENAI_KEY`
- If the base URL does not end in `/v1`, Kite normalizes it before calling `/responses`.

## Practical preflight checks

- `git status --short --branch`
- `git log --oneline -n 12`
- `command -v kt`
- `git remote -v`
