# Timesheet Sync

Synchronizes TSheets (QuickBooks Time) entries to Jira/Tempo worklogs.

## Time Splitting Algorithm

When a TSheets entry references multiple Jira tickets in its notes, time is split
in 15-minute (0.25h) increments:

1. Divide total quarter-hours evenly across tickets
2. Distribute remainders to lower-numbered tickets first
3. Total allocated time must always equal the original duration

Example: 1.25h across ABC-123, ABC-456, ABC-789
- ABC-123: 0.50h (2 quarters)
- ABC-456: 0.50h (2 quarters)
- ABC-789: 0.25h (1 quarter)

## Ticket Reference Patterns

Recognized in TSheets notes field:
- Single: `ABC-123`
- Comma-separated: `ABC-123, ABC-456`
- Slash-separated: `ABC-123/ABC-456`
- Mixed in prose: `Working on ABC-123 and DEF-456`

## Tempo API

Uses Tempo REST API v4 (https://api.tempo.io/4) with bearer token authentication.
Key endpoints:
- `GET /worklogs/user/{accountId}?from=&to=` — fetch existing worklogs
- `POST /worklogs` — create worklog
- `PUT /worklogs/{id}` — update worklog

## User Mapping

Maps TSheets users to Jira accounts by email address. The mapping is bootstrapped
from a known set and extended automatically via Jira user search API.
