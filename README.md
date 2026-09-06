# ArcRelay Automation

Local-first, linear desktop automations: **when → only if → do**.

This crate contains no UI, platform monitoring, QuickAction catalog, or mobile-only code. The desktop implements `AutomationRunner` and supplies normalized events, current capabilities, immutable action snapshots, and native execution.

## Contract

- One typed trigger; application/device targets can be selected together within that trigger.
- AND-only time-range, application-running and device-connected conditions.
- Ordered QuickAction references, inline Shell, delay and notification steps; stop on failure.
- Automatic or ask-before-run mode. A dangerous QuickAction always requires a separate unlocked-desktop confirmation.
- Definitions use optimistic revisions. Activity records preserve definition, event and action snapshots; edits and deletions do not change an in-flight run.
- Duplicate `(automation_id, event_id)` pairs create one activity. Overlap, cooldown, conditions and bounded capacity produce explicit skipped records. Self/chain origin tags are rejected.
- The time-zone-aware scheduler stores its next slot. Missed slots skip or catch up once; DST gaps are skipped and repeated local times run once.
- Interrupted queued/running work is not replayed on restart. Awaiting-confirmation records survive and resume only after confirmation.

## Local storage and bounds

`automations.sqlite3` is separate from the frozen legacy workflow database. Core tables are `automations`, `automation_runs`, `automation_step_runs`; internal tables store schedule slots and migration tombstones. Writes use transactions, foreign keys and a five-second SQLite busy timeout.

`AutomationStore::new` only validates connection options and is safe outside Tokio.
The first awaited store operation opens SQLite and initializes the schema in one
shared `OnceCell`; clones share the initialized pool. Run that first operation on
the long-lived runtime serving the store, not a temporary preparation runtime.
Initialization errors are returned and may be retried; corrupt databases are not
silently replaced. SQLx's `connect_lazy_with` is not a runtime-free constructor:
it starts background pool maintenance even before opening a connection.

Limits: 256 definitions, 64 steps/definition, 16 conditions, 128 outstanding activities, 16 executing activities. Completed history retains up to 100 entries within 30 days; pending confirmations are never pruned. Deleting a migrated definition retains its migration tombstone.

Shell: noninteractive, no automatic elevation, default 300-second timeout (maximum one hour), at most 64 KiB per output stream, bounded pipe draining, cancellation and descendant cleanup. Unix uses a process group; Windows uses `taskkill /T /F`. Inline event values are passed as environment variables, not interpolated into executable code; quote variable tokens as data. Existing QuickAction scripts use `run_shell_literal` and retain literal braces. Long-running toggle commands remain owned by the desktop ActionService.

## Checks

From the integration workspace:

```sh
cargo test -p arcrelay-automation --lib startup_store_
cargo test -p arcrelay-automation
cargo clippy -p arcrelay-automation --all-targets -- -D warnings
```

Tests cover runtime-free startup, runtime handoff, reopening persisted data,
concurrent initialization, retry and corrupt-file preservation, plus snapshots,
confirmation, cancellation, idempotency, overlap, recovery, schedules/DST,
migration tombstones, output bounds, shell failures and event injection boundaries.
Startup-constructor regressions must include plain `#[test]` coverage: running
every constructor in `#[tokio::test]` masks missing-runtime panics.
Cross-platform packaging belongs to integration CI.
