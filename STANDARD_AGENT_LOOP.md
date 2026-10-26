# Standard agent loop (multirecorder)

## 0. Frame
- Read the user's request first and decide how much of this loop it needs:
  a quick question gets steps 1–2 at most; "new feedback is in" or
  "what's next?" gets the full loop.

## 1. Orient (read-only)
- Local: current branch, uncommitted changes, `main` in sync with origin.
- Remote: open PRs and their CI status (lint, tests on Windows/macOS/Ubuntu,
  `cross-windows`), what merged since last session, open issues, and
  issues/comments updated since our last activity (`gh issue list`,
  `gh issue view N --comments`).
- After a squash merge, check that the issues it fixed were actually closed.
  Squash merges drop "Fixes #N" from commit messages, so put the closing
  keywords in the PR description too.

## 2. Analyze feedback
- Feedback arrives as GitHub issues labeled `user-feedback` from a Windows
  tester using real devices. Read every new issue and comment in full.
- For each feature in the latest release: exercised, not exercised, or
  unknown? Check the PR's unchecked test-plan items.
- Where it's "unknown", what diagnostic data is missing? The main sources
  are the session's `session.log` and the app version. Ask the tester for
  `session.log` when a report is about a recording.
- For each failure, decide whether it's our bug, the tester's environment,
  or unclear. Decode OS error codes first (e.g. `0x8889000A` =
  `AUDCLNT_E_DEVICE_IN_USE`, i.e. exclusive mode). Reproduce with a fake
  device in `tests/recorder.rs` where possible.
- Correct misunderstandings with facts (e.g. "is everything held in RAM?":
  no, capture streams to disk), measured if possible.

## 3. Triage
Priority order, following PROBLEM_STATEMENT.md ("reliability is king"):
  1. Lost or corrupted recordings, crashes, recordings that silently stop
  2. Regressions in shipped features
  3. The user's explicit request
  4. Diagnostic gaps from step 2
  5. New features and UI requests
- Propose a version scope (one PR per minor version, one commit per
  issue) and confirm it with the user. Note what's deferred and why
  (e.g. ASIO #7 needs native Windows CI).

## 4. Do
- Sync `main`, then branch `feature/vX.Y.Z`. Commit as Claude, per issue,
  with "Fixes #N" only when fully done ("Refs #N" for partial work).
- The recording path (track worker, ring buffer, raw capture) must stay
  simple and independent of new features: put new work beside it (own
  thread, failure tolerated), never inside the audio callback.
- Add a test for every bug fixed and every reliability behaviour; scripted
  fake devices cover disconnects, stalls, floods, loopback idle, etc.
- Bump the version in `Cargo.toml` and update README for user-visible changes.

## 5. Verify
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` for the
  Linux and `x86_64-pc-windows-gnu` targets, and `cargo test`. Repeat
  timing-based tests several times.
- Exercise the GUI on Xvfb (ALSA null device): record → stop → export,
  plus screenshots of changed UI.
- Cross-compile the release exe and check that it imports only system DLLs.
- List what could NOT be verified here (real devices, WASAPI behaviour,
  loopback, performance on the tester's machine) as unchecked PR test items.

## 6. Close the loop
- Push, open or update the PR (summary, closing keywords, test plan) and
  wait for CI on all five checks.
- Reply on issues with a diagnosis, workaround or plan, signed
  "— Claude (AI assistant), on behalf of the maintainer".
- Close issues fixed by merged PRs that weren't auto-closed.
- Rebuild `target/x86_64-pc-windows-gnu/release/multirecorder.exe` for the
  tester. Update memory/docs with anything non-obvious learned.

## 7. Report
- What changed, what's verified, what isn't, what the tester should try,
  what needs the user's decision, and the suggested next step.

Pushing, commenting, closing issues and merging need the user's approval;
merging is always the user's call.
