# Config Sync — Domain Expertise

## Git Merge Conflict Resolution

### Understanding Conflict Markers

When Git cannot auto-merge, it inserts conflict markers into the affected files:

```
<<<<<<< HEAD
(your local changes)
=======
(upstream changes)
>>>>>>> upstream/main
```

### Resolution Strategies

**Ours (keep local)**:
- `git checkout --ours <file>` keeps the local version entirely
- Best when local customisation is intentional and upstream changed the same lines
- Always log what was overridden so the user can review

**Theirs (keep upstream)**:
- `git checkout --theirs <file>` takes the upstream version entirely
- Best for files that should track upstream exactly (e.g. shared tooling configs)

**Manual merge**:
- Abort the merge and notify the user
- Safest when both sides have meaningful, non-overlapping changes that need human judgement

### Best Practices for Config Repos

1. **Always fetch before merge** — never merge against stale remote refs
2. **Stash before merge** — uncommitted changes block merges; stash them, merge, then pop
3. **One commit per sync** — batch all local changes into a single descriptive commit
4. **Never force-push** — config repos are shared; force-push destroys others' history
5. **Check for in-progress operations** — a previous interrupted merge/rebase can leave `.git/MERGE_HEAD` or `.git/rebase-apply/`

## Keybase Service Management

### Checking Status

```bash
keybase status
```

Returns exit code 0 if the service is running. Key fields:
- `Logged in: yes/no` — whether a user session is active
- `Service: running/not running` — whether the background service is up
- `KBFS: running/not running` — whether the filesystem layer is available

### Starting the Service

```bash
run_keybase
```

This is the recommended way to start Keybase on Linux. It launches:
1. The Keybase service daemon
2. KBFS (Keybase Filesystem) mount
3. The GUI (if available and desired)

If `run_keybase` is not available:
```bash
keybase service &
kbfsfuse &
```

### Why Keybase Matters for Config Sync

Many users store their config repos on Keybase Git (`keybase://private/user/repo`) because:
- End-to-end encrypted at rest
- No need for SSH keys or tokens — Keybase identity handles auth
- Works across devices seamlessly via KBFS
- Private repos with zero configuration

If the Keybase service is not running, `git fetch` / `git push` to `keybase://` remotes will fail with transport errors.

## Git Operations for Config Repos

### Fetch All Upstreams

```bash
git fetch --all
```

Fetches from all configured remotes. Config repos sometimes have multiple remotes (e.g. `origin` on Keybase, `github` as a mirror).

### Detecting Upstream Changes

```bash
git log HEAD..@{upstream} --oneline
```

Shows commits on the upstream tracking branch that haven't been merged locally. Empty output means local is up-to-date.

### Detecting Local Changes Not Pushed

```bash
git log @{upstream}..HEAD --oneline
```

Shows local commits not yet pushed. Empty output means nothing to push.

### Safe Stash Workflow

```bash
# Stash with descriptive message
git stash push -m "config-sync auto-stash 2026-04-03T12:00:00Z"

# Do merge operations...

# Pop the stash (re-applies changes)
git stash pop

# If pop conflicts, the stash is NOT dropped — it remains in the stash list
# You can inspect with: git stash show -p
```

### Committing All Local Changes

```bash
git add -A                    # Stage everything (new, modified, deleted)
git commit -m "descriptive message"
```

For config repos, `git add -A` is appropriate because all files in the repo are intended to be tracked. This is unlike source code repos where you'd be more selective.
