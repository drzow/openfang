---
name: self-update-skill
version: "1.0.0"
description: "Expert knowledge for OpenFang self-update: Git upstream management, cargo build profiles, daemon restart lifecycle, and rollback procedures"
author: OpenFang
tags: [self-update, build, restart, rollback, cargo, git]
tools: [shell_exec, file_read, file_write, memory_store, memory_recall]
runtime: prompt_only
---

## 1. Git Remote Management

- Checking for upstream remote: `git remote get-url upstream`
- Adding upstream: `git remote add upstream git@github.com:RightNow-AI/openfang.git`
- Fetching: `git fetch upstream`
- Counting commits behind: `git rev-list HEAD..upstream/main --count`
- Viewing pending commits: `git log HEAD..upstream/main --oneline`

### Fast-Forward Merge Strategy

- `git merge upstream/main --ff-only` — refuses if local has diverged
- Why --ff-only: for a production deployment that tracks upstream, local-only commits indicate divergence that needs human resolution, not auto-merging
- If ff-only fails: "fatal: Not possible to fast-forward, aborting." — this means the local branch has commits not on upstream

### Pushing to Origin

- `git push origin main` — best-effort after merge
- If rejected: log warning but don't abort — the local build matters more than origin sync

## 2. Cargo Build Profiles

### Release Profile

- Defined in workspace Cargo.toml: `lto = true`, `codegen-units = 1`, `strip = true`, `opt-level = 3`
- Build time: 15-30 minutes on typical hardware
- Produces smallest, fastest binary

### Release-Fast Profile

- `inherits = "release"`, `lto = "thin"`, `codegen-units = 8`
- Build time: 5-15 minutes — recommended for self-update
- Slightly larger binary but significantly faster builds

### Build Timeout Requirements

- Default `shell_exec` timeout is 30 seconds — WILL kill the build
- MUST pass `timeout_seconds: 1800` (30 minutes) to shell_exec for cargo build
- The cron schedule `timeout_secs` must be at least 2400 (40 minutes) to cover the full agent turn including build

### Cleaning Stale Artifacts

- `cargo clean -p openfang-cli -p openfang-desktop` removes only the relevant crates' artifacts
- Required before rollback rebuilds — incremental compilation with LTO can produce stale artifacts when source files revert
- Full `cargo clean` is overkill and trashes the entire target directory

### Build Target Directory

- `--profile release` produces output in `target/release/openfang`
- `--profile release-fast` produces output in `target/release-fast/openfang`
- The profile name maps directly to the subdirectory name

### Build Output Patterns

- `Compiling <crate> v<version>` — normal progress
- `Finished <profile> target(s) in <duration>` — success
- `error[E<code>]:` — compilation error, build failed
- `warning:` — non-fatal, does not fail the build (unless `-D warnings` is used)
- `error: could not compile` — build failed, check preceding errors

## 3. Daemon Restart Lifecycle

### Detecting the Running Daemon

```bash
cat ~/.openfang/daemon.json
```

Returns JSON with: `pid`, `listen_addr`, `started_at`, `version`, `platform`

CRITICAL: daemon.json is deleted during graceful shutdown (server.rs line 857). You must read it BEFORE initiating shutdown and embed all values as literals in the restart script.

### Detecting the Running Binary Path

```bash
readlink /proc/<PID>/exe
```

Returns the absolute path to the running executable. On Linux, `/proc/<PID>/exe` is a symlink to the binary. Fallback: `which openfang`.

### Graceful Shutdown

- SIGTERM triggers the shutdown_signal handler in server.rs
- The daemon runs a 10-phase graceful shutdown with 120-second total timeout
- Phases: Draining -> BroadcastingShutdown -> WaitingForAgents -> ClosingBrowsers -> ClosingMcp -> StoppingBackground -> FlushingAudit -> ClosingDatabase -> Complete
- daemon.json is removed after axum stops but before kernel.shutdown() completes

### Restart Script Design

The script is written to `~/.openfang/restart.sh` (not `/tmp` — may be noexec, and is world-writable TOCTOU risk).

All values are embedded as shell variable literals at generation time:

```bash
DAEMON_PID=12345
BINARY_PATH="/home/user/.cargo/bin/openfang"
REPO_DIR="/home/user/projects/openfang"
ROLLBACK_COMMIT="abc123def456"
KEYBASE_TEAM="drzowbot"
BUILD_PROFILE="release-fast"
```

The script is invoked via `nohup bash ~/.openfang/restart.sh` — using `bash` explicitly avoids needing execute permission on the script file.

### Why nohup Works

When the daemon shuts down, it terminates all child processes (agent loops run inside the tokio runtime). But a process launched via `nohup ... &` is:

1. Detached from the terminal
2. Immune to SIGHUP
3. Reparented to init/systemd when its parent dies

The script continues running after the daemon exits.

## 4. Rollback Procedures

### Git Rollback

```bash
git reset --hard <ROLLBACK_COMMIT>
```

This moves HEAD backward to the pre-merge commit and resets the working tree. The merge commit is discarded. The next scheduled run will re-attempt the merge.

Why NOT `git checkout HEAD~1 -- .`: This copies files from the parent commit into the working tree but does NOT move HEAD. Git history still says you're on the merge commit. Subsequent pulls re-apply the changes. Confusing and broken.

Why NOT `git revert HEAD --no-edit`: This creates a new commit that undoes the changes. Cleaner history but means the next self-update run will see "0 commits behind" and skip the update, even though the code was reverted. The upstream commits are still in the log.

### Build Artifact Cleanup Before Rollback Rebuild

```bash
cargo clean -p openfang-cli -p openfang-desktop
```

Required because incremental compilation with LTO may cache artifacts from the failed build that cause link errors or miscompilation when building from reverted source.

### Rollback in the Restart Script

If the new daemon fails health checks within 90 seconds:

1. Kill the unhealthy new daemon
2. `git reset --hard $ROLLBACK_COMMIT`
3. `cargo clean -p openfang-cli -p openfang-desktop`
4. `cargo build --profile $BUILD_PROFILE -p openfang-cli`
5. `cp` the rebuilt binary to the installed path
6. Start the reverted daemon
7. Alert via Keybase #alerts

### CRITICAL Failure: Rollback Build Fails

If the rollback rebuild also fails, no automated recovery is possible. The script posts a CRITICAL alert to Keybase with @drzow and logs to `~/.openfang/self-update.log`. Manual intervention is required.

## 5. Concurrency & Lock File

### Lock File Protocol

- Location: `~/.openfang/self-update.lock`
- Created by the hand BEFORE launching the restart script
- Removed by the restart script via `trap cleanup_lock EXIT`
- Contains: timestamp and reason

### Staleness Detection

If the lock file exists and is older than 1 hour, it is considered stale (leftover from a crashed update). The hand removes it and proceeds.

Check lock age:

```bash
find "$HOME/.openfang/self-update.lock" -mmin +60 -print 2>/dev/null
```

If output is non-empty, the lock is stale.

### Why a Lock File

- The cron scheduler fires the hand on schedule. If an update takes 30 minutes and the schedule is daily, this isn't an issue.
- But manual triggers or very short schedules could cause re-entry.
- After a restart, the new daemon reloads hand state and the hand runs Phase 0. If the restart script is still running its health check, Phase 0 must see the lock and wait.

## 6. Keybase Notification Patterns

### Notification Responsibility Split

- **The hand** posts ONLY: "update initiated, daemon will restart shortly" (before launching restart script)
- **The restart script** posts the FINAL outcome: success or failure
- This avoids false positives (hand says "success" but restart fails) and duplication

### Message Formats

**Update initiated (hand -> #info)**:
```
Self-Update Hand (<hostname>): update initiated. <N> commits from upstream. Daemon will restart shortly. <ISO timestamp>
```

**Restart success (script -> #info)**:
```
Self-Update Hand (<hostname>): update complete. Commit: <short hash>. Daemon restarted successfully. <ISO timestamp>
```

**Restart failure (script -> #alerts)**:
```
@drzow Self-Update Hand (<hostname>): restart FAILED — <reason>. Rolling back to <commit>. <ISO timestamp>
```

**Build failure (hand -> #alerts)**:
```
@drzow Self-Update Hand (<hostname>): build failed after merging <N> commits. First error: <line>. <ISO timestamp>
```

**Merge conflict (hand -> #alerts)**:
```
@drzow Self-Update Hand (<hostname>): fast-forward merge failed — local branch has diverged from upstream. Manual resolution required. <ISO timestamp>
```

**Dirty working tree (hand -> #alerts)**:
```
@drzow Self-Update Hand (<hostname>): dirty working tree — update aborted. Files: <list>. <ISO timestamp>
```

**CRITICAL rollback failure (script -> #alerts)**:
```
@drzow Self-Update Hand (<hostname>): CRITICAL — rollback build also failed. Manual intervention required. Check ~/.openfang/self-update.log. <ISO timestamp>
```

## 7. Common Failure Modes

| Failure | Symptom | Response |
|---------|---------|----------|
| Build timeout | shell_exec returns timeout error | Increase timeout_seconds to 1800 |
| Dirty working tree | `git status --porcelain` non-empty | Hard abort, alert |
| Local branch diverged | `--ff-only` fails with "Not possible to fast-forward" | Abort, alert |
| No upstream remote | `git remote get-url upstream` fails | Auto-add using upstream_url setting |
| Binary path mismatch | New binary built but old one still running | Detect via `/proc/<PID>/exe` |
| Stale lock file | Lock exists but process is gone | Auto-remove if >1 hour old |
| daemon.json deleted | Script tries to read after shutdown | Embed values as literals at generation time |
| Rollback build fails | Both new and old builds broken | CRITICAL alert, manual intervention |
| `/tmp` noexec | Script at /tmp won't execute | Script lives at ~/.openfang/, invoked via `bash` |

## 8. Knowledge Graph Entities

### Entity Types

| Type | Properties | Description |
|------|-----------|-------------|
| `managed_service` | schedule, build_profile, upstream_url | The self-update service itself |
| `update_run` | commits_merged, old_commit, new_commit, build_duration, result, timestamp | A single update execution |

### Relation Types

| Relation | From -> To | Description |
|----------|-----------|-------------|
| `executed` | managed_service -> update_run | Links service to execution |
