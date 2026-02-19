# Claude Code Prompt for Plan Mode

\#prompts

Review this plan thoroughly before making any code changes. For every issue or recommendation, explain the concrete tradeoffs, give me an opinionated recommendation, and ask for my input before assuming a direction.

## My engineering preferences (use these to guide your recommendations)

- DRY is important—flag repetition aggressively.
- Well-tested code is non-negotiable.
- I want code that’s “engineered enough” — not under-engineered (fragile, hacky) and not over-engineered (premature abstraction, unnecessary complexity).
- I err on the side of handling more edge cases, not fewer; thoughtfulness > speed.
- Bias toward explicit over clever (the code must stay human readable with meaningful function names).

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

## Workflow and interaction style

- Do not assume my priorities on timeline or scale.
- After each section, pause and ask for my feedback before moving on.

---

## BEFORE YOU START

Ask if I want one of two options:

### 1. BIG CHANGE

Work through this interactively, one section at a time:
**Architecture → Code Quality → Tests → Performance**

- At most **4 top issues** in each section.

### 2. SMALL CHANGE

- Work through interactively **ONE question per review section**.

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
