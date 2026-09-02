---
name: scheduling
description: "How to schedule cron jobs that actually deliver their results. Read this BEFORE adding, changing or debugging any cron job, scheduled task, reminder, or daily news/report feed."
metadata: {"nanobot":{"always":false}}
---

# Scheduling jobs that actually reach the user

Jobs are managed with `metis.exe cron ...` via the exec tool. The scheduler
runs **inside `metis gateway`** — if no gateway is running, nothing fires,
no matter what the job list says. That is the first thing to check when a
job "missed": look for the gateway before blaming the job.

## The one mistake that causes most "it didn't work"

A job without delivery flags runs and then **discards its result**. Nobody
receives anything, and it looks exactly like the scheduler being broken.

Wrong (result goes nowhere):

```
cron add --name news --cron "0 0 11 * * *" --message "..."
```

Right:

```
cron add --name news --cron "0 0 11 * * *" --message "..." \
  --deliver --channel telegram --to <chat_id>
```

The chat id is the user's Telegram chat id — the same value as
`channels.telegram.allowedUsers` in `~/.metis/config.json` (read it with
read_file if you do not know it). `cron add` prints a ⚠ warning when a job
has no delivery — never ignore that warning, fix the command.

## Times and timezones

- `--at` accepts an explicit zone (`2026-09-02T13:00:00Z`,
  `2026-09-02T08:00:00-05:00`) **or** naive local time
  (`2026-09-02T08:00:00`). Naive means the MACHINE's local time.
- `--cron` expressions run in **UTC**. Six fields: `0 0 11 * * *` = 11:00
  UTC daily.
- `cron list --all` prints the current time in both zones and every next-run
  with its offset. **Do arithmetic from that output, not from memory.**

## Verify after every change — do not trust your own success message

After `cron add`, the output echoes the parsed next-run time, how far away
it is, and where it delivers. **Read it.** If it says `OVERDUE` or shows the
wrong hour, the time was misparsed or miswritten — remove and re-add.

Then run `cron list --all` and confirm the job is present with the expected
Next Run. A job can also be checked end-to-end with `cron run <id>`, BUT:

- `cron run` executes in the CLI process. It proves the job's message works;
  it does **not** prove the gateway will fire it, and it does **not** test
  Telegram delivery.
- The exec tool kills commands at its timeout (default 120s). A `cron run`
  that hits that limit tells you nothing about whether the job is too slow —
  scheduled runs have no such timeout. Do not conclude "the job times out".

## Reading the list

- `every 1h` jobs repeat; `one-time` jobs run once and then show `done`.
- A `done` one-time job is finished, not stuck.
- `cron remove` of an id that does not exist FAILS — if a remove fails, the
  id was wrong; re-check with `cron list --all` instead of assuming it
  worked.

## Debugging a job the user says never arrived

Work through these in order and report which one it was:

1. Is the gateway running? No gateway → nothing fires.
2. `cron list --all` — does the job exist, with the expected Next Run?
3. Does it have `--deliver` + `--to` + `--channel`? If not, it ran into the
   void. Fix the flags; do not reschedule the same broken job.
4. Did it run? Check the job's status/last-run in the list.
5. Only after 1-4 check the message content itself with `cron run <id>`.

Never tell the user a job "should work" — verify which of the five steps
holds, then say what was found.
