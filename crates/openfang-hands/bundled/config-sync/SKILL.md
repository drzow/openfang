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

## Keybase Channel Integration

All Keybase chat operations use the JSON pipe API (`keybase chat api`) rather than the positional CLI commands. This provides structured input/output and more reliable parsing.

### Keybase Chat API Reference

**Sending a message to a team channel**:
```bash
printf '{"method":"send","params":{"options":{"channel":{"name":"%s","topic_name":"%s","members_type":"team"},"message":{"body":"%s"}}}}' "<team>" "<channel>" "<message>" | keybase chat api
```
Example:
```bash
printf '{"method":"send","params":{"options":{"channel":{"name":"%s","topic_name":"info","members_type":"team"},"message":{"body":"%s"}}}}' "drzowbot" "config-sync (myhost): pushed 3 commits to main. 2026-04-03T12:00:00Z" | keybase chat api
```
The response JSON contains `result.message` on success or `error` on failure.

**Reading recent messages from a team channel**:
```bash
echo '{"method":"read","params":{"options":{"channel":{"name":"<team>","topic_name":"<channel>","members_type":"team"},"pagination":{"num":<count>}}}}' | keybase chat api
```
Example:
```bash
echo '{"method":"read","params":{"options":{"channel":{"name":"drzowbot","topic_name":"info","members_type":"team"},"pagination":{"num":20}}}}' | keybase chat api
```
The response contains `result.messages[]`. Each message has:
- `msg.content.text.body` — the message text
- `msg.sent_at` — Unix timestamp (seconds)
- `msg.sent_at_ms` — Unix timestamp (milliseconds, higher precision)
- `msg.sender.username` — who sent it

**Listening for real-time messages**:
```bash
timeout <seconds> keybase chat api-listen 2>/dev/null || true
```
Emits one JSON object per line to stdout as messages arrive. Each line contains:
- `msg.content.text.body` — the message text
- `msg.channel.name` — the team name
- `msg.channel.topic_name` — the channel name
- `msg.sender.username` — who sent it

The `timeout` wrapper ensures the listener exits after a bounded wait. The `|| true` prevents a non-zero exit code from aborting the script.

**Listing channels in a team**:
```bash
echo '{"method":"listconvsonname","params":{"options":{"name":"<team>","members_type":"team","topic_type":"chat"}}}' | keybase chat api
```
The response contains `result.conversations[]`. Each conversation has `channel.topic_name` indicating the channel name. Use this to verify that `#info` and `#alerts` channels exist before attempting to send.

### Notification Patterns

All notifications include the hostname via `$(hostname)` to identify which instance sent the message. This is critical for multi-instance setups.

**Pull notification (posted to #info)**:
```
config-sync (<hostname>): pulled N commits from upstream. <ISO-8601-timestamp>
```

**Push notification (posted to #info)**:
```
config-sync (<hostname>): pushed N commits to <branch>. <ISO-8601-timestamp>
```

**Error alert (posted to #alerts with @mention)**:
```
@drzow config-sync (<hostname>) ERROR: <error_type>: <error_message>. <ISO-8601-timestamp>
```

The `@drzow` mention in error alerts ensures the user gets a Keybase notification for high-severity issues.

### Push Notification Listening

Other instances post push notifications to the `#info` channel. By monitoring this channel, an instance can trigger an immediate sync when another instance pushes changes, rather than waiting for its next scheduled run.

The listening strategy is a hybrid of history read and real-time listen:

**Step A -- Read recent history**:
```bash
echo '{"method":"read","params":{"options":{"channel":{"name":"drzowbot","topic_name":"info","members_type":"team"},"pagination":{"num":20}}}}' | keybase chat api
```
Parse `result.messages[]` and check each message's `msg.content.text.body` for the pattern `config-sync (<hostname>): pushed`. Filter by `msg.sent_at_ms` to only consider messages from the last hour. Compare the hostname in the message against `$(hostname)` -- only react to messages from OTHER hostnames.

**Step B -- Briefly listen for real-time**:
```bash
timeout 5 keybase chat api-listen 2>/dev/null || true
```
Parse each JSON line for matching push notifications from other hostnames.

**Filtering out own messages**: Compare the hostname in the message against the local hostname:
```bash
local_host=$(hostname)
# Only react to messages where the hostname does NOT match $local_host
```

If the message hostname matches the local hostname, ignore it -- that was our own push. Only messages from OTHER hostnames should trigger a sync.

**Timing**: Check the #info channel at the start of each activation (Phase 2.6). If a push notification from another instance is found within the lookback window (default 1 hour), proceed with an immediate sync regardless of the normal schedule. This provides event-driven sync with the daily schedule as a fallback.

### Channel Verification

Before sending any notifications, verify team membership and channel access:

1. **Check team membership**:
   ```bash
   keybase team list-memberships 2>&1 | grep <team_name>
   ```

2. **Check channel access**:
   ```bash
   echo '{"method":"listconvsonname","params":{"options":{"name":"<team_name>","members_type":"team","topic_type":"chat"}}}' | keybase chat api
   ```
   Parse `result.conversations[]` and verify that entries with `channel.topic_name` of `info` and `alerts` exist.

3. **Graceful degradation**: If channels are not accessible (team not joined, channels don't exist, Keybase not logged in), log a warning but do NOT abort the sync. Git operations are the primary mission; notifications are secondary. Store the warning:
   ```
   memory_store "config_sync_keybase_channel_warning" "channels not accessible on <team>: <timestamp>"
   ```
