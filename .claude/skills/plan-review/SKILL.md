---
name: plan-review
description: Staged review of a plan, architecture, design, or set of code changes, covering architecture, code quality, tests, and performance. Use when reviewing a plan before implementation, when working in plan mode, or when asked to review a design, an approach, or pending changes.
---

# Plan review

Review this plan thoroughly before making any code changes. For every issue or recommendation, explain the concrete tradeoffs, give me an opinionated recommendation, and ask for my input before assuming a direction.

The engineering preferences in `Claude.md` govern every recommendation below.

---

## 1. Architecture review

**Evaluate:**

- Overall system design and component boundaries.
- Dependency graph and coupling concerns.
- Data flow patterns and potential bottlenecks.
- Scaling characteristics and single points of failure.

---

## 2. Code quality review

**Evaluate:**

- Code organization and module structure.
- DRY violations—be aggressive here.
- Error handling patterns and missing edge cases (call these out explicitly).
- Technical debt hotspots.
- Areas that are over-engineered or under-engineered relative to my preferences.

---

## 3. Test review

**Evaluate:**

- Test coverage gaps (unit, integration).
- Test quality and assertion strength.
- Missing edge case coverage—be thorough.
- Untested failure modes and error paths.
- Tests must be human readable, prefer code clarity here over optimisation.
- When you modify code, add or update tests only if the change materially affects behavior, correctness, edge cases, interfaces, regression risk, or business logic.
- DO NOT add tests for trivial, cosmetic, or low-signal changes unless existing tests must be adjusted to stay consistent.
- DO NOT create test backdoors or escape hatches where you create functions in the business logic that are only meant to be used by tests

---

## 4. Performance review

**Evaluate:**

- N+1 queries and database access patterns.
- Memory-usage concerns.
- Caching opportunities.
- Slow or high-complexity code paths.

---

## For each issue you find

For every specific issue (bug, smell, design concern, or risk):

- Describe the problem concretely, with file and line references.
- Present 2–3 options, including “do nothing” where that’s reasonable.
- For each option, specify:
  - Implementation effort
  - Risk
  - Impact on other code
  - Maintenance burden
- Give me your recommended option and why, mapped to my preferences above.
- Then explicitly ask whether I agree or want to choose a different direction before proceeding.

---

## FOR EACH STAGE OF REVIEW

- Output:
  - The explanation
  - Pros and cons of each stage’s questions
  - Your opinionated recommendation and why
- Then use **AskUserQuestion**.

Additional rules:

- **NUMBER issues**.
- **Use LETTERS for options**.
- When using **AskUserQuestion**, clearly label:
  - Issue **NUMBER**
  - Option **LETTER**
- Make the **recommended option always the 1st option**.
