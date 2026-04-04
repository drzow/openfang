---
name: system-update-skill
version: "1.0.0"
description: "Expert knowledge for Debian system updates, user-local tool management, and Keybase notification patterns"
author: OpenFang
tags: [system, update, apt, debian, maintenance, keybase]
tools: [shell_exec, file_read, file_write, memory_store, memory_recall]
runtime: prompt_only
---

# System Update — Domain Expertise

## Apt Package Management

### Updating Package Lists

```bash
sudo apt update 2>&1
```

Fetches the latest package index files from all configured sources in `/etc/apt/sources.list` and `/etc/apt/sources.list.d/`. Output includes:
- `Hit:` — source was already up-to-date
- `Get:` — new data was downloaded
- `Ign:` — source was ignored (usually harmless)
- `Err:` — a source could not be reached (network issue or bad URL)

The final line reports: `N packages can be upgraded. Run 'apt list --upgradable' to see them.`

### Upgrading Packages

```bash
sudo apt upgrade -y 2>&1
```

Installs available upgrades for all currently installed packages. The `-y` flag auto-confirms. Output includes:
- `The following packages will be upgraded:` — list of packages being updated
- `N upgraded, N newly installed, N to remove and N not upgraded.` — summary line
- `The following packages have been kept back:` — held packages that require dependency changes

### Handling Lock Files

The dpkg/apt system uses lock files to prevent concurrent operations:
- `/var/lib/dpkg/lock-frontend` — front-end lock for dpkg
- `/var/lib/dpkg/lock` — dpkg database lock
- `/var/lib/apt/lists/lock` — apt package list lock
- `/var/cache/apt/archives/lock` — apt download cache lock

To check if locks are held:
```bash
sudo lsof /var/lib/dpkg/lock-frontend /var/lib/apt/lists/lock 2>&1
```

If another process holds the lock (e.g. an unattended-upgrades run or another apt instance), wait and retry. **Never forcibly remove lock files** — this can corrupt the package database.

Common lock holders:
- `unattended-upgr` — the unattended-upgrades service
- `apt` or `apt-get` — another manual apt operation
- `dpkg` — a dpkg operation in progress

### dpkg Recovery

If a previous upgrade was interrupted (power loss, killed process), the dpkg database may be in an inconsistent state. Recovery:

```bash
sudo dpkg --configure -a 2>&1
```

This completes configuration of all unpacked but unconfigured packages. Run this before retrying `apt upgrade` if you see errors like:
- `E: dpkg was interrupted, you must manually run 'sudo dpkg --configure -a'`
- `Sub-process /usr/bin/dpkg returned an error code`

### Parsing Upgrade Output

Key patterns in apt upgrade output:

| Pattern | Meaning |
|---------|---------|
| `N upgraded` | Number of packages upgraded |
| `N newly installed` | New dependency packages pulled in |
| `N to remove` | Packages being removed (rare with upgrade) |
| `N not upgraded` | Held-back packages |
| `kept back` | Packages held due to dependency changes |
| `Need to get N MB` | Download size |
| `After this operation, N MB of additional disk space` | Disk impact |

### Held Packages

Packages are "held back" when upgrading them would require installing new packages, removing existing ones, or changing dependencies in ways that `apt upgrade` won't do automatically. To see them:

```bash
apt list --upgradable 2>&1
apt-mark showhold 2>&1
```

**Do not use `apt full-upgrade` or `apt dist-upgrade` without explicit user approval** — these can remove packages and make breaking changes.

## Sudo Validation

### Passwordless Sudo Check

```bash
sudo -n true 2>&1
```

The `-n` (non-interactive) flag tells sudo not to prompt for a password. Possible outcomes:
- Exit code 0, no output — passwordless sudo works
- Exit code 1, output `sudo: a password is required` — sudo is available but needs a password
- Command not found — sudo is not installed

### Common Sudoers Configurations

Passwordless sudo is typically configured in `/etc/sudoers` or a file in `/etc/sudoers.d/`:

```
username ALL=(ALL) NOPASSWD: ALL
```

Or more restrictively for just apt:
```
username ALL=(ALL) NOPASSWD: /usr/bin/apt, /usr/bin/dpkg
```

The system update hand requires passwordless sudo because it runs unattended on a schedule. If sudo requires a password, the hand cannot function and must alert the user.

## User-Local Tool Update Commands

Each tool is updated independently. If one fails, the others should still be attempted.

### 1Password CLI (op)

```bash
# Check current version
op --version
# Example output: 2.24.0

# Update to latest
op update
# op handles its own update mechanism
# Exit code 0 on success

# Verify new version
op --version
```

### AWS CLI v2

The AWS CLI v2 does not have a self-update command. Update by re-downloading the installer:

```bash
# Check current version
aws --version
# Example output: aws-cli/2.15.0 Python/3.11.6 Linux/6.1.0 exe/x86_64.ubuntu.22

# Download latest installer
curl "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o "/tmp/awscliv2.zip"

# Unzip (overwrite existing)
cd /tmp && unzip -o awscliv2.zip

# Run installer with --update flag
sudo /tmp/aws/install --update

# Verify
aws --version

# Clean up
rm -rf /tmp/awscliv2.zip /tmp/aws
```

The `--update` flag tells the installer to update an existing installation rather than failing if one exists.

### ngrok

```bash
# Check current version
ngrok version
# Example output: ngrok version 3.5.0

# Update to latest
ngrok update

# Verify
ngrok version
```

### Rust Toolchain (rustup)

```bash
# Check current versions
rustup --version
rustc --version

# Update rustup itself and all installed toolchains
rustup update
# This updates:
#   - rustup itself
#   - stable toolchain (if installed)
#   - nightly toolchain (if installed)
#   - any other installed toolchains

# Verify
rustup --version
rustc --version
```

### Conda

```bash
# Check current version
conda --version
# Example output: conda 24.1.0

# Update conda itself first
conda update conda -y

# Then update all packages in the base environment
conda update --all -y

# Verify
conda --version
```

The `-y` flag auto-confirms prompts. Updating conda first ensures the package solver itself is current before updating other packages.

## Keybase Chat API

### Sending Messages

Use the `keybase chat api` JSON pipe interface for reliable, scriptable message delivery:

```bash
printf '{"method":"send","params":{"options":{"channel":{"name":"%s","topic_name":"%s","members_type":"team"},"message":{"body":"%s"}}}}' "<team>" "<channel>" "<message>" | keybase chat api
```

Parameters (in the JSON payload):
- `name` — the team name (e.g. `drzowbot`)
- `topic_name` — the channel within the team (e.g. `info`, `alerts`)
- `members_type` — always `"team"` for team channels
- `body` — the message text

Examples:
```bash
# Success summary to #info
printf '{"method":"send","params":{"options":{"channel":{"name":"%s","topic_name":"%s","members_type":"team"},"message":{"body":"%s"}}}}' "drzowbot" "info" "System Update Hand: update complete. 12 packages upgraded. All tools current." | keybase chat api

# Alert with mention to #alerts
printf '{"method":"send","params":{"options":{"channel":{"name":"%s","topic_name":"%s","members_type":"team"},"message":{"body":"%s"}}}}' "drzowbot" "alerts" "@drzow System Update Hand: sudo requires a password — updates cannot run." | keybase chat api
```

### Verifying Team Membership

```bash
keybase team list-memberships 2>&1
```

Check that the configured team appears in the output. If not, Keybase notifications will fail.

### Checking Channel Existence

Keybase does not have a direct "list channels" CLI command. If `keybase chat api` returns an error with "channel not found", log the error. The team admin needs to create the channel.

### Verifying Keybase Status

```bash
keybase status 2>&1
```

Key fields in the output:
- `Logged in: yes` — a user session is active
- `Service: running` — the background daemon is up
- `Username:` — the authenticated Keybase user

If the service is not running:
```bash
run_keybase 2>&1
```

This starts the Keybase service, KBFS, and GUI. If `run_keybase` is unavailable:
```bash
keybase service &
sleep 5
keybase status 2>&1
```

## Common Failure Modes

### Lock Contention

**Symptom**: `E: Could not get lock /var/lib/dpkg/lock-frontend`

**Cause**: Another package manager process is running (commonly `unattended-upgrades`).

**Response**: Wait 60 seconds and retry. Up to 3 retries. If still locked, log the holding process (from `lsof` output) and defer the update.

### Network Failures

**Symptom**: `Err:` lines in apt update output, messages like `Could not resolve host`, `Failed to fetch`, `Temporary failure resolving`.

**Response**: Wait 30 seconds and retry once. If the retry also fails, the network is likely down — log the error and skip system package updates, but still attempt user-local tool updates (some may work from cached installers or different CDNs).

### Held Packages

**Symptom**: `The following packages have been kept back:` in apt upgrade output.

**Cause**: Upgrading these packages would require installing new dependencies or removing existing packages that `apt upgrade` won't do.

**Response**: Log the held package names. Do not run `apt full-upgrade` or `apt dist-upgrade` — these can remove packages unexpectedly. Report held packages to the user for manual review.

### Partial Upgrades

**Symptom**: `E: dpkg was interrupted`, `Sub-process /usr/bin/dpkg returned an error code`, packages in "half-configured" or "half-installed" state.

**Cause**: A previous apt/dpkg operation was interrupted.

**Response**:
1. Run `sudo dpkg --configure -a` to complete pending configurations
2. Retry `sudo apt upgrade -y`
3. If still failing, log the error with full output for diagnosis

### Individual Tool Update Failures

**Symptom**: A specific tool's update command returns non-zero or outputs an error.

**Common causes**:
- Tool not installed (command not found)
- Network connectivity issue reaching the tool's update server
- Permission issue (tool installed in a location requiring different privileges)
- Tool-specific issue (e.g. conda solver conflict)

**Response**: Log the error for that specific tool, record the failure, and continue updating the remaining tools. One tool failure should never block the others.

## Knowledge Graph Entities

### Entity Types

| Type | Description | Example Properties |
|------|-------------|--------------------|
| `managed_system` | The local system being maintained | `schedule`, `auto_upgrade`, `update_user_tools` |
| `update_run` | A single update execution | `packages_upgraded`, `held_packages`, `tools_updated`, `timestamp` |
| `managed_tool` | A user-local tool being tracked | `name`, `current_version`, `last_updated` |

### Relation Types

| Relation | From | To | Description |
|----------|------|----|-------------|
| `updated_on` | `managed_system` | `update_run` | Links a system to an update execution |
| `manages` | `managed_system` | `managed_tool` | Links a system to the tools it maintains |
| `updated_in` | `managed_tool` | `update_run` | Links a tool to the run that updated it |
