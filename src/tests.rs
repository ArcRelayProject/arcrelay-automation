use crate::*;
use chrono::{TimeZone, Utc};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Runner {
    action: Mutex<Option<ActionSnapshot>>,
    executions: Mutex<Vec<String>>,
    locked: Mutex<bool>,
}
#[async_trait::async_trait]
impl AutomationRunner for Runner {
    async fn capabilities(&self) -> Vec<Capability> {
        [
            "manual",
            "session.confirm",
            "system.locked",
            "system.started",
            "schedule",
            "shell.system",
            "shell.sh",
            "notification.mobile",
        ]
        .iter()
        .map(|id| Capability {
            id: (*id).into(),
            available: true,
            reason: None,
            remedy: None,
        })
        .collect()
    }
    async fn environment(&self) -> EnvironmentState {
        EnvironmentState {
            locked: *self.locked.lock().unwrap(),
            ..Default::default()
        }
    }
    async fn snapshot_action(&self, _: &str) -> std::result::Result<ActionSnapshot, String> {
        self.action
            .lock()
            .unwrap()
            .clone()
            .ok_or("quick action was deleted".into())
    }
    async fn execute_action(&self, snapshot: &ActionSnapshot, _: CancellationToken) -> StepResult {
        self.executions.lock().unwrap().push(snapshot.name.clone());
        StepResult::default()
    }
    async fn notify(&self, title: &str, _: &str, _: bool) -> StepResult {
        self.executions.lock().unwrap().push(title.into());
        StepResult::default()
    }
}
fn runner() -> Arc<Runner> {
    Arc::new(Runner {
        action: Mutex::new(Some(ActionSnapshot {
            name: "original".into(),
            definition: serde_json::json!({"version":1}),
            requires_confirmation: false,
            requires_unlocked_session: false,
            requirements: vec![],
        })),
        ..Default::default()
    })
}

// Keep this a plain test: #[tokio::test] would hide the desktop's synchronous
// startup context and let a runtime-dependent constructor regress unnoticed.
#[test]
fn startup_store_constructs_without_tokio_and_opens_on_first_use() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("automations.sqlite3");
    let store = AutomationStore::new(&format!("sqlite://{}", path.display())).unwrap();
    assert!(!path.exists(), "construction must not open the database");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let saved = store.save(definition()).await.unwrap();
        assert_eq!(store.get(&saved.id).await.unwrap().name, saved.name);
        assert_eq!(store.list().await.unwrap().len(), 1);
    });
    assert!(path.is_file());
}

#[test]
fn startup_store_survives_preparation_runtime_and_reopens_existing_data() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}",
        directory.path().join("automations.sqlite3").display()
    );
    let preparation = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let store = preparation.block_on(async { AutomationStore::new(&url).unwrap() });
    drop(preparation);

    let backend = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let saved = backend.block_on(async move { store.save(definition()).await.unwrap() });
    drop(backend);

    // A warm launch must also construct outside Tokio, then retain old records.
    assert!(tokio::runtime::Handle::try_current().is_err());
    let reopened = AutomationStore::new(&url).unwrap();
    let backend = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    backend.block_on(async move {
        let persisted = reopened.get(&saved.id).await.unwrap();
        assert_eq!(persisted.revision, saved.revision);
        assert_eq!(persisted.name, saved.name);
        assert_eq!(reopened.list().await.unwrap().len(), 1);
    });
}

#[tokio::test]
async fn startup_store_clones_share_first_initialization() {
    let store = AutomationStore::new("sqlite::memory:").unwrap();
    let clone = store.clone();
    let (first, second) = tokio::join!(store.save(definition()), clone.save(definition()));
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(clone.get(&first.id).await.unwrap().id, first.id);
    assert_eq!(store.get(&second.id).await.unwrap().id, second.id);
    assert_eq!(store.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn startup_store_failed_open_can_retry_without_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let store = AutomationStore::new(&format!(
        "sqlite://{}",
        missing.join("automations.sqlite3").display()
    ))
    .unwrap();
    assert!(store.list().await.is_err());
    std::fs::create_dir(&missing).unwrap();
    assert!(store.list().await.unwrap().is_empty());
    let saved = store.save(definition()).await.unwrap();
    assert_eq!(store.get(&saved.id).await.unwrap().id, saved.id);
}

#[tokio::test]
async fn startup_store_does_not_replace_corrupt_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("automations.sqlite3");
    let original = b"not a sqlite database";
    std::fs::write(&path, original).unwrap();
    let store = AutomationStore::new(&format!("sqlite://{}", path.display())).unwrap();
    assert!(store.list().await.is_err());
    assert!(store.list().await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn startup_store_invalid_configuration_returns_error_without_tokio() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    assert!(AutomationStore::new("sqlite::memory:?mode=invalid").is_err());
}

#[tokio::test]
async fn missed_schedule_skips_or_catches_up_exactly_once() {
    for catch_up in [false, true] {
        let runner = runner();
        let store = AutomationStore::new("sqlite::memory:").unwrap();
        let engine = AutomationEngine::new(store.clone(), runner.clone());
        let mut d = definition();
        d.enabled = true;
        d.trigger = AutomationTrigger::Schedule {
            time: "09:00".into(),
            weekdays: vec![1, 2, 3, 4, 5, 6, 7],
            timezone: "UTC".into(),
            catch_up,
        };
        let d = engine.save(d).await.unwrap();
        store
            .advance_schedule(&d.id, (Utc::now() - chrono::Duration::days(2)).timestamp())
            .await
            .unwrap();
        engine.tick().await.unwrap();
        let rows = engine.activities(Some(&d.id), 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        let activity = terminal(&engine, &rows[0].id).await;
        assert_eq!(
            activity.status,
            if catch_up {
                ActivityStatus::Succeeded
            } else {
                ActivityStatus::Skipped
            }
        );
        assert_eq!(
            runner.executions.lock().unwrap().len(),
            usize::from(catch_up)
        );
        engine.tick().await.unwrap();
        assert_eq!(engine.activities(Some(&d.id), 100).await.unwrap().len(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn failed_step_stops_sequence_and_literal_scripts_keep_braces() {
    let runner = runner();
    let engine = engine(runner.clone());
    let mut d = definition();
    d.steps.insert(
        0,
        AutomationStep::Shell {
            shell: ShellKind::Sh,
            script: "exit 17".into(),
            working_directory: None,
            timeout_seconds: 5,
        },
    );
    let id = engine.test(d, None).await.unwrap();
    let activity = terminal(&engine, &id).await;
    assert_eq!(activity.status, ActivityStatus::Failed);
    assert_eq!(activity.steps[0].result.exit_code, Some(17));
    assert!(runner.executions.lock().unwrap().is_empty());
    let output = run_shell_literal(
        ShellKind::Sh,
        "printf '%s' '{{ literal }}'",
        None,
        5,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(output.stdout, "{{ literal }}");
    assert!(output.error.is_none());
}

#[test]
fn byte_bounded_results_and_direction_specific_variables() {
    let output = StepResult {
        stdout: "中".repeat(65536),
        stderr: String::from_utf8_lossy(&[0xff; 65536]).into(),
        ..Default::default()
    }
    .bounded();
    assert!(output.stdout.len() <= 65536 && output.stderr.len() <= 65536);
    let mut d = definition();
    d.trigger = AutomationTrigger::Transfer {
        received: false,
        device_ids: vec![],
        file_kinds: vec![],
    };
    d.steps = vec![AutomationStep::Notification {
        title: "Sent".into(),
        body: "{{ event.file.directory }}".into(),
        send_to_connected_devices: false,
    }];
    assert!(validate(&d).is_err());
    if let AutomationTrigger::Transfer { received, .. } = &mut d.trigger {
        *received = true;
    }
    assert!(validate(&d).is_ok());
}
fn definition() -> AutomationDefinition {
    let mut d = AutomationDefinition::new(AutomationTrigger::Manual);
    d.name = "test".into();
    d.steps.push(AutomationStep::QuickAction {
        action_id: "action".into(),
    });
    d
}
fn engine(runner: Arc<Runner>) -> AutomationEngine {
    AutomationEngine::new(AutomationStore::new("sqlite::memory:").unwrap(), runner)
}
async fn terminal(engine: &AutomationEngine, id: &str) -> AutomationActivity {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let a = engine.activity(id).await.unwrap();
            if a.status.terminal() || a.status == ActivityStatus::AwaitingConfirmation {
                return a;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn immutable_action_and_definition_snapshots() {
    let runner = runner();
    let engine = engine(runner.clone());
    let mut d = definition();
    d.run_mode = RunMode::AskBeforeRun;
    let d = engine.save(d).await.unwrap();
    let id = engine.run(&d.id).await.unwrap();
    assert_eq!(
        engine.activity(&id).await.unwrap().status,
        ActivityStatus::AwaitingConfirmation
    );
    runner.action.lock().unwrap().as_mut().unwrap().name = "edited".into();
    let mut changed = d.clone();
    changed.steps = vec![AutomationStep::Notification {
        title: "replacement".into(),
        body: "".into(),
        send_to_connected_devices: false,
    }];
    engine.save(changed).await.unwrap();
    engine.confirm(&id).await.unwrap();
    let a = terminal(&engine, &id).await;
    assert_eq!(a.status, ActivityStatus::Succeeded);
    assert_eq!(a.definition.revision, d.revision);
    assert_eq!(*runner.executions.lock().unwrap(), vec!["original"]);
}
#[tokio::test]
async fn dangerous_actions_always_wait_and_cannot_confirm_while_locked() {
    let runner = runner();
    runner
        .action
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .requires_confirmation = true;
    let engine = engine(runner.clone());
    let mut d = definition();
    d.steps.push(AutomationStep::Notification {
        title: "later".into(),
        body: "".into(),
        send_to_connected_devices: false,
    });
    let d = engine.save(d).await.unwrap();
    let id = engine.run(&d.id).await.unwrap();
    assert_eq!(
        terminal(&engine, &id).await.status,
        ActivityStatus::AwaitingConfirmation
    );
    assert!(runner.executions.lock().unwrap().is_empty());
    *runner.locked.lock().unwrap() = true;
    assert!(engine.confirm(&id).await.is_err());
    *runner.locked.lock().unwrap() = false;
    engine.confirm(&id).await.unwrap();
    assert!(engine.confirm(&id).await.is_err());
    assert_eq!(
        terminal(&engine, &id).await.status,
        ActivityStatus::Succeeded
    );
    assert_eq!(
        *runner.executions.lock().unwrap(),
        vec!["original", "later"]
    );
}
#[tokio::test]
async fn duplicate_events_are_one_activity() {
    let engine = engine(runner());
    let mut d = definition();
    d.trigger = AutomationTrigger::System {
        event: SessionEvent::Started,
    };
    d.run_mode = RunMode::AskBeforeRun;
    let d = engine.save(d).await.unwrap();
    let event = AutomationEvent::new("system.started");
    let first = engine.dispatch(event.clone()).await.unwrap();
    let second = engine.dispatch(event).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(engine.activities(Some(&d.id), 100).await.unwrap().len(), 1);
}
#[tokio::test]
async fn overlapping_runs_skip_and_conditions_have_reasons() {
    let engine = engine(runner());
    let mut d = definition();
    d.run_mode = RunMode::AskBeforeRun;
    let d = engine.save(d).await.unwrap();
    engine.run(&d.id).await.unwrap();
    let id = engine.run(&d.id).await.unwrap();
    assert_eq!(
        engine.activity(&id).await.unwrap().reason.as_deref(),
        Some("the previous run has not finished")
    );
    let condition = AutomationCondition::DeviceConnected {
        device_id: "phone".into(),
        connected: true,
    };
    assert!(condition_failure(&[condition], &EnvironmentState::default(), Utc::now()).is_some());
}
#[tokio::test]
async fn deleted_action_fails_instead_of_skipping() {
    let runner = runner();
    let engine = engine(runner.clone());
    let d = engine.save(definition()).await.unwrap();
    *runner.action.lock().unwrap() = None;
    assert_eq!(engine.preflight(&d).await[0].code, "action.missing");
    let id = engine.run(&d.id).await.unwrap();
    assert_eq!(
        engine.activity(&id).await.unwrap().status,
        ActivityStatus::Failed
    );
}
#[tokio::test]
async fn optimistic_revisions_prevent_lost_updates() {
    let engine = engine(runner());
    let d = engine.save(definition()).await.unwrap();
    engine.save(d.clone()).await.unwrap();
    assert!(matches!(
        engine.save(d).await,
        Err(AutomationError::Conflict)
    ));
}
#[tokio::test]
async fn recovery_interrupts_runs_but_retains_confirmations() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("automation.db").display());
    let store = AutomationStore::new(&url).unwrap();
    let engine = AutomationEngine::new(store.clone(), runner());
    let mut d = definition();
    d.run_mode = RunMode::AskBeforeRun;
    let d = engine.save(d).await.unwrap();
    let id = engine.run(&d.id).await.unwrap();
    let mut pending = engine.activity(&id).await.unwrap();
    pending.id = "interrupted".into();
    pending.event.id = "other".into();
    pending.status = ActivityStatus::Running;
    store.insert_activity(&pending).await.unwrap();
    let restarted = AutomationEngine::new(AutomationStore::new(&url).unwrap(), runner());
    assert_eq!(restarted.recover().await.unwrap(), 1);
    assert_eq!(
        restarted.activity("interrupted").await.unwrap().status,
        ActivityStatus::Interrupted
    );
    assert_eq!(
        restarted.activity(&id).await.unwrap().status,
        ActivityStatus::AwaitingConfirmation
    );
}
#[tokio::test]
async fn cancel_waiting_is_durable_and_never_runs() {
    let runner = runner();
    let engine = engine(runner.clone());
    let mut d = definition();
    d.run_mode = RunMode::AskBeforeRun;
    let d = engine.save(d).await.unwrap();
    let id = engine.run(&d.id).await.unwrap();
    engine.cancel(&id).await.unwrap();
    assert_eq!(
        engine.activity(&id).await.unwrap().status,
        ActivityStatus::Canceled
    );
    assert!(engine.confirm(&id).await.is_err());
    assert!(runner.executions.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_cannot_resurrect_a_confirmation_request() {
    let runner = runner();
    runner
        .action
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .requires_confirmation = true;
    let engine = engine(runner.clone());
    for _ in 0..32 {
        let d = engine.save(definition()).await.unwrap();
        let id = engine.run(&d.id).await.unwrap();
        engine.cancel(&id).await.unwrap();
        let activity = terminal(&engine, &id).await;
        assert_eq!(activity.status, ActivityStatus::Canceled);
        assert!(engine.confirm(&id).await.is_err());
    }
    assert!(runner.executions.lock().unwrap().is_empty());
}
#[tokio::test]
async fn lock_incompatible_actions_fail_preflight() {
    let runner = runner();
    runner
        .action
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .requires_unlocked_session = true;
    let engine = engine(runner);
    let mut d = definition();
    d.trigger = AutomationTrigger::System {
        event: SessionEvent::Locked,
    };
    assert_eq!(engine.preflight(&d).await[0].code, "session.locked");
    assert!(engine.save(d).await.is_err());
}
#[test]
fn stable_application_identity_and_multiple_targets() {
    let app = ApplicationIdentity {
        id: "us.zoom.xos".into(),
        name: "Zoom".into(),
        path: "/Applications/zoom.us.app".into(),
    };
    let trigger = AutomationTrigger::Application {
        event: ApplicationEvent::Foreground,
        apps: vec![app],
    };
    let mut event = AutomationEvent::new("application.foreground");
    event
        .variables
        .insert("event.app.id".into(), "us.zoom.xos".into());
    event
        .variables
        .insert("event.app.name".into(), "different display name".into());
    assert!(trigger.matches(&event));
    event
        .variables
        .insert("event.app.id".into(), "helper".into());
    assert!(!trigger.matches(&event));
}
#[test]
fn overnight_conditions_belong_to_starting_weekday() {
    let c = AutomationCondition::TimeRange {
        weekdays: vec![1],
        start: "22:00".into(),
        end: "02:00".into(),
        timezone: "UTC".into(),
    };
    let t = Utc.with_ymd_and_hms(2026, 9, 8, 1, 0, 0).unwrap();
    assert!(condition_failure(std::slice::from_ref(&c), &EnvironmentState::default(), t).is_none());
    assert!(condition_failure(
        &[c],
        &EnvironmentState::default(),
        t + chrono::Duration::days(1)
    )
    .is_some());
}
#[test]
fn scheduler_skips_nonexistent_dst_time() {
    let t = AutomationTrigger::Schedule {
        time: "02:30".into(),
        weekdays: vec![7],
        timezone: "America/New_York".into(),
        catch_up: false,
    };
    let next = next_schedule(&t, Utc.with_ymd_and_hms(2026, 3, 8, 0, 0, 0).unwrap()).unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 15, 6, 30, 0).unwrap());
}

#[tokio::test]
async fn deleting_a_migrated_automation_does_not_remove_migration_tombstone() {
    let engine = engine(runner());
    let mut d = definition();
    d.legacy_id = Some("old".into());
    let d = engine.save(d).await.unwrap();
    engine.delete(&d.id).await.unwrap();
    assert!(engine.migrated("old").await.unwrap());
    assert!(engine.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn self_generated_events_are_recorded_as_skipped() {
    let engine = engine(runner());
    let mut d = definition();
    d.trigger = AutomationTrigger::System {
        event: SessionEvent::Started,
    };
    let d = engine.save(d).await.unwrap();
    let mut event = AutomationEvent::new("system.started");
    event.origin_automation_id = Some(d.id);
    let ids = engine.dispatch(event).await.unwrap();
    assert_eq!(
        engine.activity(&ids[0]).await.unwrap().status,
        ActivityStatus::Skipped
    );
}

#[test]
fn presence_trigger_and_condition_are_stable_and_matchable() {
    let trigger = AutomationTrigger::Presence {
        state: PresenceEvent::UnknownPresent,
    };
    let mut event = AutomationEvent::new("presence.unknownPresent");
    event
        .variables
        .insert("event.presence.state".into(), "unknownPresent".into());
    event
        .variables
        .insert("event.presence.faceCount".into(), "1".into());
    event
        .variables
        .insert("event.presence.ownerSimilarity".into(), "0.42".into());
    assert_eq!(trigger.capability(), "presence.unknownPresent");
    assert!(trigger.matches(&event));
    assert!(available_variables(&trigger).contains(&"event.presence.ownerSimilarity"));

    let condition = AutomationCondition::Presence {
        state: PresenceEvent::OwnerPresent,
    };
    let matching = EnvironmentState {
        presence_state: Some(PresenceEvent::OwnerPresent),
        ..Default::default()
    };
    assert!(condition_failure(&[condition.clone()], &matching, Utc::now()).is_none());
    assert!(condition_failure(&[condition], &EnvironmentState::default(), Utc::now()).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn shell_is_bounded_preserves_exit_code_and_event_data_is_not_code() {
    let mut event = AutomationEvent::new("application.foreground");
    event
        .variables
        .insert("event.app.name".into(), "$(printf INJECTED); data".into());
    let result = run_shell(
        ShellKind::Sh,
        "printf '%s' \"{{ event.app.name }}\"; exit 7",
        None,
        10,
        &event,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.exit_code, Some(7));
    assert_eq!(result.stdout, "$(printf INJECTED); data");
    assert!(result.error.is_some());
    let result = run_shell(
        ShellKind::Sh,
        "head -c 100000 /dev/zero",
        None,
        10,
        &event,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.stdout.len(), 65536);
}
#[cfg(unix)]
#[tokio::test]
async fn shell_cancel_stops_a_running_process_group() {
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
    });
    let result = run_shell(
        ShellKind::Sh,
        "sleep 30 & wait",
        None,
        30,
        &AutomationEvent::new("test"),
        cancel,
    )
    .await;
    assert_eq!(result.error.as_deref(), Some("run canceled"));
}
