# Buildkite CI

Replaces the GitHub Actions workflows in `.github/workflows/` (`tests.yml`,
`latency.yml`, `docs-drift.yml`). The step definitions live in
[pipeline.yml](pipeline.yml); each step runs the matching script in
[scripts/](scripts/).

## One-time setup

1. **Create the pipeline** in the Buildkite UI: repository
   `git@github.com:Peppy-bot/peppyos.git`, default branch `dev` (the repo's
   default). The initial step must target the self-hosted queue, or the
   upload job runs on a Buildkite-hosted agent (the cluster's default queue):

   ```yaml
   steps:
     - label: ":pipeline:"
       command: buildkite-agent pipeline upload
       agents:
         queue: Self-hosted
   ```

   pipeline.yml pins the same queue for every step it uploads (top-level
   `agents:`). The value is the exact, case-sensitive queue key (queue page →
   Settings), which is also what the agent's `tags="queue=..."` config points
   at; uploads are rejected if it names a queue the cluster does not have.

2. **GitHub triggers** (Pipeline → Settings → GitHub) — this replaces the
   `on:` blocks of the old workflows:
   - Trigger builds on **pushes** and **pull requests**.
   - Leave branch limiting empty and instead enable **filter builds using a
     conditional** with:

     ```
     build.branch == "main" || build.pull_request.base_branch =~ /^(main|dev)$/
     ```

3. **Cancel Intermediate Builds** (Pipeline → Settings → Builds) — replaces
   the `concurrency: cancel-in-progress` groups. Buildkite scopes cancellation
   per branch, which for pull-request builds means per PR.

4. **Agent** — run a Buildkite agent on the machine that hosted the
   self-hosted Actions runner; it needs the same toolchain (cargo, pixi, uv,
   the container runtime) plus the **`gh` CLI**, which docs-drift uses in
   place of the `peter-evans/create-pull-request` action. Keep one agent per
   host so the latency medians are not skewed by a concurrent tests job. Do
   not configure shallow clones (`--depth` in `BUILDKITE_GIT_CLONE_FLAGS`) —
   the doc scripts and the scripts-changed check need history
   (`fetch-depth: 0` parity).

5. **Secret** — docs-drift needs the bot token that was
   `secrets.PEPPY_BOT_TOKEN`: either create a Buildkite secret named
   `peppy_bot_token`, or export `PEPPY_BOT_TOKEN` from the agent's
   `environment` hook.

6. **Branch protection** — swap the required status checks from the Actions
   check names to the Buildkite contexts `tests`, `latency`, and `docs-drift`
   (set per step via `notify: github_commit_status` in pipeline.yml). Only
   `tests` should be required on ordinary PRs: `latency` and `docs-drift`
   report solely on the dev → main release PR.

## Behavior differences vs. Actions

- **docs-drift** ran only on PR opened/reopened; Buildkite does not expose the
  PR action type, so the script now runs on every release-PR build and is
  idempotent: it force-pushes `auto/docs-update-<n>` and only creates the
  follow-up PR when none is open. Net effect: the docs PR stays in sync with
  later pushes instead of going stale.
- Actions tested the synthetic PR **merge commit**; Buildkite builds the PR
  **head commit** (what `latency.yml`/`docs-drift.yml` already did via
  `ref: pull_request.head.sha`).
- The `scripts/` change detection (was `dorny/paths-filter`) diffs against the
  merge base for PR builds, and against `HEAD^` for pushes to `main` — one
  merge lands at a time on a protected branch, so that first-parent diff is
  the pushed change.

## Cutover

Once the pipeline is green on a PR and on `main`, delete `.github/workflows/`
and update branch protection as in step 6.
