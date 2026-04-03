# Forgetty Development Harness Methodology

## CRITICAL: All feature work MUST follow this three-phase subagent workflow.

This methodology applies to every feature request, bug fix involving new functionality, refactor, or integration task — regardless of perceived complexity. Do NOT skip phases. Do NOT combine phases into a single context. Each phase runs as a separate subagent with a clean context window.

---

## How It Works

When starting work on a task from `docs/harness/BACKLOG.md`, you act as the **Orchestrator**. You NEVER write application code yourself. You coordinate three subagents sequentially, passing structured artifacts between them via files in `docs/harness/`.

Each task gets its own directory for artifacts: `docs/harness/T-{NUM}/`. Create it before starting. Do NOT delete previous task artifacts — they serve as historical reference.

---

## MANDATORY: Read Architecture Decisions First

**Before doing ANYTHING else — before reading BACKLOG, before reading SESSION_STATE, before writing any code or spec — read this file:**

```
docs/architecture/ARCHITECTURE_DECISIONS.md
```

This file contains locked decisions that override BACKLOG.md, CLAUDE.md, and older docs when they conflict. Every agent (Planner, Builder, QA) in every phase must read it. No exceptions.

---

## Task Selection

1. Read `docs/architecture/ARCHITECTURE_DECISIONS.md` — locked decisions (see above)
2. Read `docs/harness/BACKLOG.md` to find the next uncompleted task
3. Tasks are ordered by priority — always pick the first `[ ]` (unchecked) task
4. Do NOT skip tasks or reorder without explicit user approval

---

## Phase 1: Planning

Launch a subagent with this role:

**Identity:** You are a senior product engineer and architect. You think in user outcomes, not implementation details.

**Input:** The task description from BACKLOG.md (passed verbatim) + awareness of the existing codebase structure. Always read CLAUDE.md first for project context.

**Instructions:**
- Read `docs/architecture/ARCHITECTURE_DECISIONS.md` — locked decisions that override everything else
- Read `CLAUDE.md` for project architecture and conventions
- Read the codebase to understand current patterns
- Expand the task into a full spec with:
  - **Goal:** One paragraph describing what this achieves for the user
  - **User stories:** Concrete "As a user, I want X so that Y" items
  - **Acceptance criteria:** Testable, binary (pass/fail) conditions — be specific about expected behaviors, edge cases, and error states
  - **Technical approach:** High-level architecture decisions (which files to modify/create, which patterns to follow) — do NOT over-specify implementation
  - **Out of scope:** Explicitly list what this task does NOT include
  - **Risk flags:** Anything that could break existing functionality
  - **Human testing instructions:** Specific steps the user should take to manually verify (commands to run, what to look for on screen)
- Match existing code conventions

**Output:** Save the complete spec to `docs/harness/T-{NUM}/SPEC.md`

**On completion:** The orchestrator reads the spec and verifies it's coherent before proceeding.

---

## Phase 2: Build

Launch a subagent with this role:

**Identity:** You are a senior full-stack engineer. You write clean, production-grade code. You follow existing project conventions exactly.

**Input:** `docs/harness/SPEC.md` + full codebase access. Always read CLAUDE.md first.

**Instructions:**
- Read `docs/architecture/ARCHITECTURE_DECISIONS.md` — locked decisions that override everything else
- Read `CLAUDE.md` for architecture context and FFI patterns
- Read `docs/harness/SPEC.md` completely before writing any code
- Read existing code in the areas you'll modify to absorb patterns
- Implement the spec feature by feature, in logical order
- Do NOT evaluate your own work quality
- If the spec is ambiguous, document the decision in `docs/harness/T-{NUM}/BUILDER_NOTES.md`
- After implementation, run:
  - `cargo check --workspace` — must pass
  - `cargo build --release` — must pass
  - `cargo fmt --all`
  - Crash test: `for i in $(seq 1 10); do timeout 2 cargo run --release 2>&1 | grep -c "segmentation" && echo "CRASH $i" || echo "OK $i"; done`

**Output:** Working implementation + optional `docs/harness/BUILDER_NOTES.md`

**On completion:** The orchestrator verifies the app builds and doesn't crash before proceeding.

---

## Phase 3: QA / Evaluation (HUMAN-IN-THE-LOOP)

Launch a subagent with this role:

**Identity:** You are a ruthless, skeptical QA engineer. You are NOT generous. **You do NOT try to automate visual/interactive testing.** You prepare the test plan and ASK THE USER to execute it.

**Input:** `docs/harness/T-{NUM}/SPEC.md` + the codebase (with Builder's changes) + `docs/harness/T-{NUM}/BUILDER_NOTES.md` (if exists).

**Instructions:**
- Read `docs/architecture/ARCHITECTURE_DECISIONS.md` — verify the implementation doesn't violate any locked decision
- Read the spec to understand what was supposed to be built
- Read the Builder's code changes for code quality review
- For each acceptance criterion, prepare **human testing instructions**:
  - Exact command to run
  - What to type / click / press
  - What the user should see on screen
  - Ask the user to take a screenshot if visual verification is needed
- **ASK THE USER** to execute each test and report results (pass/fail + screenshot if needed)
- Do NOT try to run the GUI app yourself — you cannot see the screen
- You CAN run non-visual tests (cargo check, cargo test, grep for code patterns)
- After receiving user test results, assign scores (1-10): Completeness, Functionality, Code quality, Robustness
- A score below 7 in ANY category means FAIL
- Save report to `docs/harness/T-{NUM}/QA_REPORT.md`
- **If ALL scores >= 7:** Update BACKLOG.md (mark task `[x]`), SESSION_STATE.md (point to next task), and `~/Home/Forge/forge/CROSS_PRODUCT.md` (add Bulletin Board entry — see Phase 5 for required content)

---

## Phase 4: Fix Cycle (if QA fails)

If ANY QA score is below 7:
1. Launch new Builder with `T-{NUM}/SPEC.md` + `T-{NUM}/QA_REPORT.md` — fix every FAIL
2. Launch new QA to re-evaluate (same human-in-the-loop process)
3. Maximum 3 fix cycles. If still failing, save to `docs/harness/T-{NUM}/FINAL_STATUS.md` and report to user.

---

## Phase 5: Task Completion

After QA passes (all scores >= 7):
1. Commit all changes with descriptive message
2. Update `docs/harness/BACKLOG.md` — mark the task as `[x]` completed
3. Save session state to `docs/harness/SESSION_STATE.md`
4. **Update `~/Home/Forge/forge/CROSS_PRODUCT.md`** — add a dated Bulletin Board entry under the most recent entry. Include:
   - Task ID + one-line summary of what shipped
   - Any protocol decisions, API changes, or integration gotchas relevant to other teams (especially Android)
   - Current desktop milestone status (so Android and other product leads stay in sync)
   This step is MANDATORY. Other product teams read CROSS_PRODUCT.md to stay coordinated.
5. Report to user with human testing instructions from QA

---

## Session Continuity

If a session ends mid-workflow, save to `docs/harness/SESSION_STATE.md`:
- Current task from BACKLOG.md
- Current phase (Planning/Build/QA/Fix)
- What's been completed
- Next steps

The next session reads this file and resumes.

---

## File Structure

```
docs/harness/
├── METHODOLOGY.md       # This file — workflow rules (permanent)
├── BACKLOG.md           # Ordered task backlog (permanent, updated per task)
├── SESSION_STATE.md     # Cross-session checkpoint (updated per task)
├── T-001/
│   ├── SPEC.md          # Phase 1 output
│   ├── BUILDER_NOTES.md # Phase 2 output (optional)
│   └── QA_REPORT.md     # Phase 3 output
├── T-002/
│   ├── SPEC.md
│   ├── BUILDER_NOTES.md
│   └── QA_REPORT.md
└── ...                  # One directory per task (never deleted)
```
