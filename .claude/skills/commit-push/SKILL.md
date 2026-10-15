---
name: commit-push
description: Commit the current changes in FluxCapture and push them to GitHub (origin/main, https://github.com/VictorCavalcant/FluxCapture), checking for merge conflicts with the remote before pushing. Use whenever the user asks to commit, push, "subir", "enviar pro git", "fazer commit", sync with GitHub, or publish the current work.
---

# commit-push — commit, check for conflicts, push to `origin/main`

The user invoking this skill authorizes the commit and the push: run it end to end without asking for
confirmation. **Stop and ask** only when a check below fails (gate, conflict, suspicious file) — never
work around one.

Remote: `origin` = `https://github.com/VictorCavalcant/FluxCapture.git`, branch `main`. History is kept
linear (rebase onto `origin/main`, no merge commits). Use the Bash tool (POSIX syntax) for the git
commands below.

## Steps

1. **Pre-flight: the working tree must not be mid-conflict.**
   ```bash
   git rev-parse --abbrev-ref HEAD                      # must be main
   ls "$(git rev-parse --git-path MERGE_HEAD)" "$(git rev-parse --git-path rebase-merge)" \
      "$(git rev-parse --git-path rebase-apply)" 2>/dev/null   # must print nothing
   git diff --name-only --diff-filter=U                 # unmerged paths: must print nothing
   git remote get-url origin                            # must be the URL above
   ```
   - On another branch: tell the user and ask before switching (it may hold unfinished work).
   - A merge/rebase in progress or unmerged paths: go to **Resolving conflicts** below.
   - No `origin`: `git remote add origin https://github.com/VictorCavalcant/FluxCapture.git`.

2. **Look at what will be committed.** `git status --porcelain -uall` and `git diff --stat`.
   Never commit (unstage/ignore them and tell the user):
   - `dist/`, `target/`, recordings (`*.mkv`, `*.mp4`), `*.log`, test configs, `portable.txt`/`config\`
     next to an .exe;
   - secrets: `.env`, `*.pem`, `*.key`, `*.pfx`, `*.p12`, tokens or passwords inside files;
   - any file over 10 MB (`git ls-files -o -m --exclude-standard | xargs -d '\n' du -k | sort -rn | head`).
   If such a file is a pattern that will come back, add it to `.gitignore` in the same commit.
   Nothing to commit and nothing ahead of `origin/main` → report "already up to date" and stop.

3. **Leftover conflict markers.** None may reach a commit:
   ```bash
   git grep --untracked -nE '^(<<<<<<<|>>>>>>>)( |$)|^=======$' -- ':!*.md'   # must print nothing
   ```
   Any hit → stop, show file:line, ask the user. (`git diff --check` also reports them, mixed with
   whitespace warnings; those warnings alone don't block.)

4. **Gates.** If anything under `apps/`, `crates/`, `xtask/`, `Cargo.toml`, `Cargo.lock`, `.cargo/`,
   `clippy.toml` or `rustfmt.toml` changed, `cargo xtask ci` must pass (CLAUDE.md). It takes a while: run
   it in the background, wait for the notification, and don't pipe it through `head`/`tail` (that hides
   the exit code). A failure → fix the cause (or report it) — never commit around a failing gate.
   Docs-only changes skip this step.

5. **Commit.** Stage explicitly (`git add -A` only after step 2 cleared the list), then commit with a
   message in English: subject ≤ 72 chars in the imperative mood saying what changed and why
   ("Fix audio drift when the device sample rate changes"), a blank line, an optional short body, then
   the attribution trailer from the system reminder:
   ```bash
   git commit -F - <<'EOF'
   <subject>

   <body>

   Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
   EOF
   ```
   Never `--no-verify`, never `--amend` a commit that is already on `origin`.

6. **Check the remote for conflicts.**
   ```bash
   git fetch origin
   git rev-list --left-right --count HEAD...origin/main 2>/dev/null   # "<ahead> <behind>"
   ```
   - `origin/main` doesn't exist (empty repo) → go to step 7.
   - `behind` = 0 → go to step 7.
   - `behind` > 0 → **predict** the result before touching the tree:
     ```bash
     git merge-tree --write-tree --name-only HEAD origin/main; echo "exit=$?"
     ```
     - `exit=0` → no conflict: `git rebase origin/main`. If the incoming commits touched any path from
       step 4's list, re-run `cargo xtask ci` before pushing.
     - `exit=1` → conflicts in the files it lists. Stop and go to **Resolving conflicts**.

7. **Push** (never `--force` / `--force-with-lease` on `main`):
   ```bash
   git push -u origin main
   ```
   Rejected as non-fast-forward (someone pushed meanwhile) → back to step 6, once. A second rejection
   or an auth error → stop and report the exact message (the user may need to sign in to GitHub via
   Git Credential Manager in their own terminal).

8. **Verify and report.** `git status -sb` must show `## main...origin/main` with no `[ahead …]`.
   Report in one or two lines: the commit hash + subject, that the push succeeded, and the link
   `https://github.com/VictorCavalcant/FluxCapture/commit/<hash>`. If code changed and `dist\*.exe`
   was not rebuilt in this task, also run the `build-exe` skill (it is independent of the push).

## Resolving conflicts

Never resolve with `-X ours`/`-X theirs`, `git checkout --ours/--theirs` on whole files, or by deleting
the other side's work. Show the user the conflicting files and both sides
(`git log --oneline HEAD..origin/main -- <file>`, `git diff HEAD...origin/main -- <file>`), then:

- If the user asks you to resolve: `git rebase origin/main`, edit each conflicted file keeping both
  intents, `git add <file>`, `git rebase --continue`; then run `cargo xtask ci` (always, after a manual
  resolution) and continue at step 7.
- To back out at any point: `git rebase --abort` (or `git merge --abort`) restores the previous state.
