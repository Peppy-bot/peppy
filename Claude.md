# Project instructions

## My engineering preferences (use these to guide your recommendations)

- DRY is important—flag repetition aggressively.
- Use the "Parse, don't validate" pattern when applicable
- Well-tested code is non-negotiable.
- Happy path is always on the left, avoid nested structures if possible
- I want code that’s “engineered enough” — not under-engineered (fragile, hacky) and not over-engineered (premature abstraction, unnecessary complexity).
- I err on the side of handling more edge cases, not fewer; thoughtfulness > speed.
- Bias toward explicit over clever (the code must stay human readable with meaningful function names).
- Do not leave legacy code behind or code that is meant to support previous version of the code.
- When you write comments or documentation, stop using em-dash `—`, this is a recurring pattern that you make and immediately jumps at users as being your coding style.
- Push back hard against design implementation that you think are fundamentally wrong and explain your reasoning
- Never open or merge PRs in the remote repository without my consent

---

## Workflow and interaction style

- Do not assume my priorities on timeline or scale.
- Review this plan thoroughly before making any code changes. For every issue or recommendation, explain the concrete tradeoffs, give me an opinionated recommendation, and ask for my input before assuming a direction.
- When reviewing a plan, a design, or pending changes, follow the `plan-review` skill (`.claude/skills/plan-review/SKILL.md`).

---

## Approach

- Think before acting. Read existing files before writing code.
- Be concise in output but thorough in reasoning.
- Prefer editing over rewriting whole files.
- Do not re-read files you have already read unless the file may have changed.
- Test your code before declaring done.
- No sycophantic openers or closing fluff.
- Keep solutions simple and direct. No over-engineering, but if a big refactor is needed to keep code easier to reason about, proceed.
- If unsure: say so and ask user questions in doubt.
- Never guess or invent file paths.
- User instructions always override this file.
