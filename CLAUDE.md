# CLAUDE.md

`AGENTS.md` is a symlink to this file, so every agent reads the same rules.

# Claude-specific notes

## Purpose

This file defines repository structure and working rules only. Do not duplicate
feature specifications, deployment state, hardware values, or current progress
here. Those details belong in project documentation and dated worklogs.

## Repository structure

- `README.md`: project overview and entry points.
- `src/`: application source code and embedded web assets.
- `docs/`: design documents, architecture, plans, specifications, and runbooks.
- `worklogs/`: dated records of implementation, testing, validation, progress,
  limitations, and next steps.
- `scripts/`: setup, build, deployment, and operational automation.
- `systemd/`: service and timer definitions.
- `tools/`: standalone developer tools, each a separate Cargo workspace (for
  example `tools/timelapse/`, the Mac-side timelapse builder).
- `Cargo.toml` and `Cargo.lock`: Rust package and locked dependency metadata.
- `setup.md`: host setup and commissioning guidance.
- `verify.sh`: read-only host verification.

Keep new files in the appropriate directory:

- Put design proposals, implementation plans, and technical decisions in
  `docs/`.
- Put dated feature work and verification records in `worklogs/` using a name
  such as `YYYY-MM-DD-short-topic.md`.
- Put product code in `src/`, automation in `scripts/`, and service definitions
  in `systemd/`.
- Do not add project plans or worklogs to the repository root.

## Sources of truth

Before changing the project:

1. Read `README.md`.
2. Read the relevant documents under `docs/`.
3. Read the latest relevant entries under `worklogs/`.
4. Inspect the current source, tests, scripts, and configuration involved.

Use design documents for intended behavior and worklogs for observed progress
and validation history. If they disagree, do not guess: identify the mismatch
and resolve or document it as part of the work.

## Required feature workflow

Every feature or substantial fix must follow this order:

1. **Understand:** establish scope, constraints, risks, and acceptance criteria.
2. **Plan tests first:** create or update a dated worklog before implementation.
   Record the test plan, expected behavior, failure cases, and required target
   environment.
3. **Implement:** make the smallest focused change that satisfies the accepted
   criteria. Add or update automated tests with the code.
4. **Verify:** run the relevant checks and test the actual behavior in the
   required environment. Record commands and observed results; never infer a
   pass from source inspection alone.
5. **Document:** update affected design, operational, or user documentation in
   `docs/` and other established documentation files.
6. **Close the worklog:** record changed files, test results, observed runtime
   behavior, unresolved limitations, and next steps.
7. **Report:** declare the feature complete only after implementation,
   verification, and documentation are complete. Then ask the user to perform
   their acceptance verification.

If required validation cannot be performed, state that the work is implemented
but unverified. Do not call the feature complete.

## Worklog requirements

A feature worklog should include:

- date and concise status;
- objective and acceptance criteria;
- test plan written before implementation;
- implementation summary and files changed;
- validation environment, commands, and observed results;
- failures encountered and how they were resolved;
- remaining limitations, risks, and follow-up work;
- user-verification steps when applicable.

Record facts only. Keep these states distinct: planned, implemented, tested,
deployed, hardware-validated, soak-tested, and user-accepted.

## Change principles

- Whenever possible, use the local Git repository as the file-change tracking
  system. Inspect `git status` and relevant diffs before and after changes; do not
  commit, push, or publish branches unless the user explicitly requests it.
- Make focused changes; avoid unrelated refactors and formatting churn.
- Preserve established architecture, safety constraints, and compatibility
  unless the task explicitly changes them.
- Prefer bounded resource use, explicit error handling, recoverable operations,
  and useful logs.
- Keep platform-specific code isolated so supported development environments
  continue to work.
- Add regression coverage for defects and tests for new behavior.
- Do not change locked dependencies, toolchains, deployment settings, or system
  configuration unless required by the task and covered by the plan.
- Never expose, copy, log, or commit credentials, private keys, tokens, or other
  secrets.
- Do not perform deployment, reboot, destructive operations, or privileged host
  changes without explicit user approval.

## Validation and completion

- Run the least expensive relevant checks first, followed by target-specific and
  integration checks described in the project documentation.
- Report exact checks run and distinguish passed, failed, skipped, and
  unavailable checks.
- Do not claim deployment, hardware behavior, persistence, or reliability
  without direct observation in the appropriate environment.
- Re-read the diff before completion and confirm code, tests, docs, and worklog
  agree.
- A feature is complete only when its acceptance criteria pass, documentation is
  current, the worklog is closed, and remaining limitations are explicit.

# Agent Rules & Execution Protocol

## Autonomous Execution & Momentum
- **Bias toward action:** When instructed to inspect, plan, or start a task, chain your tool calls continuously through the entire workflow (reading -> planning -> implementing -> testing) without stopping for intermediate conversational permission.
- **No idle pauses:** Never conclude a turn with standalone text like "Shall we begin?", "Ready to start", or an outline of next steps without immediately triggering the next tool call.
- **Tool chaining:** If a plan is formulated, proceed in the exact same turn to create the worklog and apply the first file edits.

## Decision Handling & User Prompts
- **Strict tool usage for questions:** Never ask open-ended permission questions in plain markdown text. When you genuinely need user direction or architectural input, you MUST use the `ask_followup_question` tool.
- **Actionable options:** Whenever invoking `ask_followup_question`, always provide 2 to 3 distinct, ready-to-select choices. For example:
  - Option 1: "Proceed with implementation immediately."
  - Option 2: "Adjust the plan / modify scope first."
  - Option 3: "Run verification checks on existing code before changing anything."
- **Pre-approved actions:** Reading project files, creating worklogs, modifying code in `src/`, and running local read-only commands (`cargo check`, `cargo test`, `git status`, `git diff`) require no conversational confirmation. Proceed autonomously.
- **Hard gates:** Only pause for confirmation on destructive actions: `git commit`, `git push`, branch deletion, file deletion, system-level modifications, or remote deployments.

## Completion Gatekeeper
- **Strict completion barrier:** Calling `attempt_completion` after merely reading documents, summarizing requirements, or writing plans is strictly prohibited.
- **Required completion criteria:** `attempt_completion` may only be called when all of the following conditions are met:
  1. Code or configuration files have actually been created or updated.
  2. Local tests, builds, or scripts have executed in the terminal and reported passing results (do not assume success from code inspection alone).
  3. Associated documentation in `docs/` has been aligned.
  4. A dated worklog in `worklogs/` has been closed with observed test results and diff summaries.
- If a task cannot be verified due to missing host dependencies or hardware, explicitly report the work as implemented but unverified, document the limitation in the worklog, and use `ask_followup_question` to determine next steps.

## Workflow & Repository Discipline
- **Source of truth:** Inspect `README.md`, relevant specs under `docs/`, and the latest file under `worklogs/` before altering functionality.
- **Plan before implementation:** For any feature or non-trivial fix, initialize a dated worklog (`worklogs/YYYY-MM-DD-<topic>.md`) outlining acceptance criteria and a concrete test plan prior to modifying `src/`.
- **Minimal footprint:** Keep changes tightly scoped to accepted criteria. Avoid sweeping refactors or formatting churn.
- **Diff inspection:** Always run and review `git diff` prior to closing a task to ensure no secrets, unintended edits, or temporary debug logs are left behind.

## Tooling & Command Preferences

- **Prioritize Ripgrep (`rg`):** Always prefer `rg` over slow built-in search tools, `grep`, or `find`. It is installed and available in the environment.
- **Code & String Search:** Use `rg -n --no-heading "<pattern>" [path]` to find function definitions, types, or references. Always restrict context with `-C <lines>` (e.g., `-C 3`) and avoid dumping massive outputs into the terminal.
- **File Discovery:** Use `rg --files | grep "<pattern>"` (or `fd "<pattern>"` if available) to locate files across the project rather than recursively listing directories.
- **Avoid Context Blowouts:** Never read an entire large file (>200 lines) with `read_file` just to inspect a small section. First locate the line numbers using `rg -n`, then read only the relevant target slice.
- **Respect Ignored Paths:** Trust `rg`'s default `.gitignore` filtering; do not run searches with `--no-ignore` or `--hidden` unless explicitly searching hidden config files.

## Self-Reflection & Environment Learning

- **Failure Analysis:** If a terminal command fails due to a non-existent utility (`command not found`), invalid flags, or platform mismatches, do not blindly retry the command. Analyze the stderr output and identify the root cause immediately.
- **Persist Fixes to `AGENTS.md`:** When you discover that a command or parameter fails and find the correct alternative, update `AGENTS.md` in the same turn under `## Environment and Tooling Constraints`:
  - Add a single, concise bullet point detailing the invalid command, the reason for failure, and the validated replacement.
  - Format: `- Do not use \`<failed-cmd>\` (<reason>); use \`<working-cmd>\` instead.`
  - Append to the list cleanly; never overwrite, reorder, or delete existing project rules or structure definitions.
- **Consult Prior Learnings:** Always inspect `## Environment and Tooling Constraints` in `AGENTS.md` before invoking unfamiliar CLI tools to prevent repeating previously logged errors.

## Environment and Tooling Constraints

- Do not use `orca worktree set --comment ...` without a selector (`orca worktree set --help` lists `--worktree <selector>` as required); use `orca worktree set --worktree current --comment ...` instead.
- Do not use `git push origin main` (GitHub ruleset GH013: changes to `main` must go through a pull request with 2 required status checks); push a branch and open a pull request instead.
- Do not use `$var:r`-style expansions such as `testsrc2=s=$sz:r=24` in zsh (zsh treats `:r` as a history modifier and strips it); use `${sz}` or run the command under `bash -c` instead.
- Do not nest a `<<'EOF'` heredoc inside another `<<'EOF'` heredoc (the inner `EOF` line ends the outer one and zsh parses the rest as commands); give the outer heredoc a distinct delimiter such as `<<'PYEOF'` instead.
- Do not use `timeout <secs> <cmd>` on the Mac (GNU coreutils `timeout` is not installed: `command not found`); for SSH use `-o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=2` and the Bash tool's own timeout instead.
- Do not use `rg` in commands run on the Pi over SSH (not installed there: `rg: command not found`); use `grep -E` on the Pi and keep `rg` for the Mac checkout.
- Do not put several SSH options in one shell variable such as `S="-o BatchMode=yes -o ConnectTimeout=10"; ssh $S host` (zsh does not word-split unquoted variables, so ssh gets one bad argument: `keyword batchmode extra arguments at end of line`); write the `-o` options out inline instead.
- Do not use `node --test tests/web` in CI (the ubuntu-24.04 runner's Node 22 treats a bare directory as a module path: `Cannot find module '.../tests/web'`; only newer Node, such as the Mac's 26, discovers tests in it); use `node --test tests/web/*.test.js` instead.

## Orca Worktree Completion Rules
- When the task, PR, or assigned scope is finished:
  1. Kill all background watchers, servers, or lingering subshells immediately (never leave dangling PTY processes).
  2. Use the Orca CLI to update the worktree status:
     `orca worktree set --worktree current --comment "Completed: <one-line summary>"`
  3. Cleanly exit your turn and yield execution with exit code 0.
