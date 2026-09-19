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
- Documentation and code comments describe the code exactly as it is today, nothing else. Never refer to a previous state of the code (what it used to do, what changed, migration notes) and never announce a future one ("a future release will", "breaking change ahead", "not yet implemented"). If a behaviour does not exist in the code you are documenting, it does not appear in the docs; when it lands, the docs are rewritten in the present tense as if it had always been that way.
- When you write comments or documentation, stop using em-dash `—`, this is a recurring pattern that you make and immediately jumps at users as being your coding style.
- Push back hard against design implementation that you think are fundamentally wrong and explain your reasoning
- Never merge PRs in the remote repository without my consent
- Never add "Co-Authored-By" trailers crediting an LLM (e.g., Claude) to commit messages; commits are authored by me alone
- When tests are written, never make them time-based, they should be deterministic and should not depend on the speed of the host
- When opening a PR, always use the repository's default branch as the base branch.

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
- When unsure about a big design decision: say so and AskUserQuestion, do not ask those questions if it's about a small implementation detail.
- Never guess or invent file paths.
- User instructions always override this file.
