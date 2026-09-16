# Contributing to Kite

Kite is a small Rust command-line tool that runs Git. Prefer direct functions, ordinary owned values, and names that explain the workflow. Add an abstraction when it removes a real source of confusion or repeated mistakes.

## Run it locally

Install stable Rust and Git, then use:

```sh
cargo build --locked
cargo run -- --help
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 .github/scripts/test_prepare_release.py
```

Use a disposable Git repository when trying commands that save, land, or undo. The compiled program is `target/debug/kt`. The test suite creates temporary repositories, local remotes, and fake AI/GitHub responses; it needs no account credentials or live AI calls.

## Find the code

| File or directory | Responsibility |
| --- | --- |
| `src/main.rs` | Command arguments, dispatch, quicksave, and branch switching |
| `src/git.rs` | Git subprocesses, refs, saved history, and exact file paths |
| `src/land.rs` | Plan, preview, and execute a local land |
| `src/land/commits.rs` | Build commit groups and check hook edits stay within their files |
| `src/land/state.rs` | Persist the transaction and read older recovery markers |
| `src/land/recovery.rs` | Undo saves and lands; recover interrupted operations |
| `src/land/stash.rs` | Preserve and restore uncommitted work during dirty lands |
| `src/land/publish.rs` | Publish with explicit protection for remote work |
| `src/ai.rs` | AI configuration, requests, retries, and response extraction |
| `src/synth.rs` | Commit planning prompt and validated file groups |
| `src/diff.rs` | Changed files and bounded diff context |
| `src/pr.rs` | PR lookup, drafting, preview, and creation or refresh |
| `src/pr/guidance.rs` | Find PR templates and dedicated writing skills |
| `src/ui.rs` | Terminal prompts, progress, and shared messages |
| `tests/` | Tests that run the actual CLI against disposable repositories |
| `.github/scripts/` | Release metadata preparation and its tests |

Focused tests usually live beside the code. Landing's larger test collection is in `src/land/tests.rs`. Shared unit-test repository helpers live in `src/test_support.rs`.

## Make a change

For a bug, first reproduce the user's sequence with the CLI and inspect the resulting files, staging, branch, and history. Keep a regression test for the behavior that failed. Check normal use and the failure path your change affects.

The important promises are concrete:

- A failed hook returns users to their original branch or detached commit with every save intact. Keep hook edits so users can save and retry.
- Landing preserves the saved file tree except for changes committed by successful hooks to the current group's files. Check staging before and after hooks so another group's files cannot slip in. AI suggests messages and file groups; Git state and file coverage stay under program control.
- Record recovery information before changing history or stashing work. A crash must leave enough information for `kt undo`.
- Ref changes use the expected old commit. New work from another process or person must survive.

Keep these promises visible in the code. Recovery state uses separate types for separate phases so required values do not become a collection of optional fields. Use simple owned strings where borrowing would complicate an API. Avoid adding a generic framework for one caller.

When changing prompts, inspect actual plans and prose using [Reviewing AI output](docs/ai-output-evaluation.md). Passing parser tests does not establish writing quality. Keep instructions separate from repository evidence, and never treat source text or a skill's operational directions as permission to execute commands.

Update the README and `skills/use-kite` when command behavior changes. The shipped skill teaches agents how to operate Kite; PR writing skills supply prose guidance to `kt pr`. They serve different purposes.
