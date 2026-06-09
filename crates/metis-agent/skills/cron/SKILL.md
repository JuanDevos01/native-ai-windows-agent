---
name: cron
description: Schedule reminders and recurring tasks using Metis's built-in cron.
---

# Cron

Metis has a **built-in scheduler**. Use it for ALL scheduling — do NOT use Windows
Task Scheduler / `schtasks`, `crontab`, or `systemd` timers.

There is no `cron` tool. You schedule jobs by running the `metis` CLI through the
`exec` tool (use the same `metis` binary that runs you; use its full path if `metis`
is not on PATH).

## Two modes

1. **Reminder** — a fixed message delivered to a chat (`--deliver`).
2. **Task** — the message is a prompt; you (the agent) execute it each time and,
   optionally, deliver the result.

## Commands

Add a recurring job (standard 5-field cron expression):
```
metis cron add --name "morning-report" --message "Summarize overnight emails" --cron "0 9 * * *"
```

Add an interval job (seconds):
```
metis cron add --name "stars" --message "Check repo stars and report" --every 600
```

Add a one-shot job (ISO 8601):
```
metis cron add --name "call-reminder" --message "Remind me to call Bob" --at "2026-03-01T09:00:00"
```

Deliver the result to a channel (e.g. Telegram):
```
metis cron add --name "daily" --message "Daily standup summary" --cron "0 8 * * 1-5" --deliver --channel telegram --to <chat_id>
```

Manage jobs:
```
metis cron list --all          # list all jobs (including disabled)
metis cron run <ID>            # trigger a job now
metis cron enable <ID>         # enable a job
metis cron enable <ID> --disable   # disable a job
metis cron remove <ID>        # delete a job
```

Job IDs are the 8-character hex shown by `metis cron list`.

## Time expressions

| User says            | Flag                    |
|----------------------|-------------------------|
| every 20 minutes     | `--every 1200`          |
| every hour           | `--every 3600`          |
| every day at 8am     | `--cron "0 8 * * *"`    |
| weekdays at 5pm      | `--cron "0 17 * * 1-5"` |
| once at a date/time  | `--at "2026-03-01T09:00:00"` |

Jobs persist across restarts in the cron store, and each task job runs as a prompt to you.
