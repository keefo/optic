# AI Agent Instructions

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