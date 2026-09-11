use crate::*;
use chrono::{TimeZone, Utc};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{broadcast, watch, Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

#[async_trait::async_trait]
pub trait AutomationRunner: Send + Sync {
    async fn record_origin(&self, _automation_id: &str) {}
    async fn capabilities(&self) -> Vec<Capability>;
    async fn environment(&self) -> EnvironmentState;
    async fn application_available(
        &self,
        _app: &ApplicationIdentity,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
    async fn snapshot_action(&self, id: &str) -> std::result::Result<ActionSnapshot, String>;
    async fn execute_action(
        &self,
        snapshot: &ActionSnapshot,
        cancel: CancellationToken,
    ) -> StepResult;
    async fn notify(&self, title: &str, body: &str, mobile: bool) -> StepResult;
}

struct Inner {
    store: AutomationStore,
    runner: Arc<dyn AutomationRunner>,
    gate: Mutex<()>,
    active: Mutex<HashMap<String, (String, CancellationToken)>>,
    capacity: Semaphore,
    events: broadcast::Sender<AutomationActivity>,
    configuration: watch::Sender<u64>,
}
#[derive(Clone)]
pub struct AutomationEngine(Arc<Inner>);

impl AutomationEngine {
    pub fn new(store: AutomationStore, runner: Arc<dyn AutomationRunner>) -> Self {
        Self(Arc::new(Inner {
            store,
            runner,
            gate: Mutex::new(()),
            active: Mutex::new(HashMap::new()),
            capacity: Semaphore::new(16),
            events: broadcast::channel(128).0,
            configuration: watch::channel(0).0,
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<AutomationActivity> {
        self.0.events.subscribe()
    }
    pub fn subscribe_configuration(&self) -> watch::Receiver<u64> {
        self.0.configuration.subscribe()
    }
    pub async fn list(&self) -> Result<Vec<AutomationDefinition>> {
        self.0.store.list().await
    }
    pub async fn get(&self, id: &str) -> Result<AutomationDefinition> {
        self.0.store.get(id).await
    }
    pub async fn activities(
        &self,
        id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AutomationActivity>> {
        self.0.store.activities(id, limit).await
    }
    pub async fn activity(&self, id: &str) -> Result<AutomationActivity> {
        self.0.store.activity(id).await
    }
    pub async fn capabilities(&self) -> Vec<Capability> {
        self.0.runner.capabilities().await
    }
    pub async fn migrated(&self, id: &str) -> Result<bool> {
        self.0.store.migrated(id).await
    }
    pub async fn preflight(&self, definition: &AutomationDefinition) -> Vec<AutomationIssue> {
        let mut issues = vec![];
        if let Err(error) = validate(definition) {
            issues.push(AutomationIssue {
                code: "configuration".into(),
                message: error.to_string(),
                remedy: "edit".into(),
                step_index: None,
            });
        }
        let capabilities = self.capabilities().await;
        let mut require = |id: String, step_index| {
            if !capabilities.iter().any(|c| c.id == id && c.available) {
                let c = capabilities.iter().find(|c| c.id == id);
                issues.push(AutomationIssue {
                    code: id,
                    message: c.and_then(|c| c.reason.clone()).unwrap_or_else(|| {
                        "this feature is not supported on the current system".into()
                    }),
                    remedy: c
                        .and_then(|c| c.remedy.clone())
                        .unwrap_or_else(|| "edit".into()),
                    step_index,
                });
            }
        };
        require(definition.trigger.capability(), None);
        if definition.run_mode == RunMode::AskBeforeRun {
            require("session.confirm".into(), None);
        }
        for condition in &definition.conditions {
            if matches!(condition, AutomationCondition::ApplicationRunning { .. }) {
                require("application.started".into(), None);
            }
            if let AutomationCondition::Presence { state } = condition {
                require(format!("presence.{}", state.token()), None);
            }
        }
        for (index, step) in definition.steps.iter().enumerate() {
            match step {
                AutomationStep::Shell { shell, .. } => require(
                    format!(
                        "shell.{}",
                        match shell {
                            ShellKind::System => "system",
                            ShellKind::Sh => "sh",
                            ShellKind::Bash => "bash",
                            ShellKind::Zsh => "zsh",
                            ShellKind::Powershell => "powershell",
                        }
                    ),
                    Some(index),
                ),
                AutomationStep::Notification {
                    send_to_connected_devices: true,
                    ..
                } => require("notification.mobile".into(), Some(index)),
                _ => {}
            }
        }
        for (index, step) in definition.steps.iter().enumerate() {
            if let AutomationStep::QuickAction { action_id } = step {
                match self.0.runner.snapshot_action(action_id).await {
                    Err(message) => issues.push(AutomationIssue {
                        code: "action.missing".into(),
                        message,
                        remedy: "actions".into(),
                        step_index: Some(index),
                    }),
                    Ok(action)
                        if action.requires_unlocked_session
                            && matches!(
                                definition.trigger,
                                AutomationTrigger::System {
                                    event: SessionEvent::Locked | SessionEvent::Sleeping
                                }
                            ) =>
                    {
                        issues.push(AutomationIssue {
                            code: "session.locked".into(),
                            message: format!(
                                "cannot run \"{}\" while the system is locked or asleep",
                                action.name
                            ),
                            remedy: "changeTrigger".into(),
                            step_index: Some(index),
                        })
                    }
                    Ok(action) => {
                        let mut requirements = action.requirements.clone();
                        if action.requires_confirmation {
                            requirements.push("session.confirm".into());
                        }
                        for id in &requirements {
                            if !capabilities.iter().any(|c| &c.id == id && c.available) {
                                let capability = capabilities.iter().find(|c| &c.id == id);
                                issues.push(AutomationIssue {
                                    code: id.clone(),
                                    message: capability
                                        .and_then(|c| c.reason.clone())
                                        .unwrap_or_else(|| {
                                            "this action is not supported on the current system"
                                                .into()
                                        }),
                                    remedy: capability
                                        .and_then(|c| c.remedy.clone())
                                        .unwrap_or_else(|| "edit".into()),
                                    step_index: Some(index),
                                });
                            }
                        }
                    }
                }
            }
        }
        let mut apps = vec![];
        if let AutomationTrigger::Application { apps: targets, .. } = &definition.trigger {
            apps.extend(targets);
        }
        for condition in &definition.conditions {
            if let AutomationCondition::ApplicationRunning { app, .. } = condition {
                apps.push(app);
            }
        }
        for app in apps {
            if let Err(message) = self.0.runner.application_available(app).await {
                issues.push(AutomationIssue {
                    code: "application.missing".into(),
                    message,
                    remedy: "edit".into(),
                    step_index: None,
                });
            }
        }
        issues
    }
    pub async fn save(&self, definition: AutomationDefinition) -> Result<AutomationDefinition> {
        let _guard = self.0.gate.lock().await;
        if definition.revision == 0 && self.list().await?.len() >= 256 {
            return Err(AutomationError::Invalid(
                "no more than 256 automations can be saved".into(),
            ));
        }
        if definition.enabled {
            let issues = self.preflight(&definition).await;
            if !issues.is_empty() {
                return Err(AutomationError::Invalid(
                    issues
                        .iter()
                        .map(|i| i.message.as_str())
                        .collect::<Vec<_>>()
                        .join("；"),
                ));
            }
            if let AutomationTrigger::Hotkey { shortcut } = &definition.trigger {
                if self.list().await?.iter().any(|d| d.id != definition.id && d.enabled && matches!(&d.trigger, AutomationTrigger::Hotkey { shortcut: other } if other == shortcut)) { return Err(AutomationError::Invalid("the shortcut is already used by another automation".into())); }
            }
        }
        let result = self.0.store.save(definition).await?;
        self.0.configuration.send_modify(|v| *v += 1);
        Ok(result)
    }
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let mut d = self.get(id).await?;
        d.enabled = enabled;
        self.save(d).await?;
        Ok(())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        let _guard = self.0.gate.lock().await;
        self.0.store.delete(id).await?;
        self.0.configuration.send_modify(|v| *v += 1);
        Ok(())
    }
    pub async fn clear_activities(&self) -> Result<()> {
        self.0.store.prune(true).await
    }
    pub async fn dispatch(&self, event: AutomationEvent) -> Result<Vec<String>> {
        let _guard = self.0.gate.lock().await;
        let mut ids = vec![];
        for definition in self.list().await? {
            if definition.enabled && definition.trigger.matches(&event) {
                ids.push(self.enqueue(definition, event.clone(), false).await?);
            }
        }
        Ok(ids)
    }
    pub async fn run(&self, id: &str) -> Result<String> {
        let _guard = self.0.gate.lock().await;
        self.enqueue(self.get(id).await?, AutomationEvent::new("manual"), true)
            .await
    }
    /// Tests do not save/enable the definition and never bypass action confirmation.
    pub async fn test(
        &self,
        definition: AutomationDefinition,
        event: Option<AutomationEvent>,
    ) -> Result<String> {
        validate(&definition)?;
        let _guard = self.0.gate.lock().await;
        self.enqueue(
            definition,
            event.unwrap_or_else(|| AutomationEvent::new("test")),
            true,
        )
        .await
    }
    async fn persist(&self, activity: &AutomationActivity) -> Result<()> {
        self.0.store.save_activity(activity).await?;
        let _ = self.0.events.send(activity.clone());
        Ok(())
    }
    async fn enqueue(
        &self,
        definition: AutomationDefinition,
        event: AutomationEvent,
        manual: bool,
    ) -> Result<String> {
        let mut activity = AutomationActivity {
            id: uuid::Uuid::new_v4().to_string(),
            automation_id: definition.id.clone(),
            definition: definition.clone(),
            event: event.clone(),
            status: ActivityStatus::Queued,
            reason: None,
            created_at: Utc::now(),
            finished_at: None,
            steps: vec![],
            confirmed_steps: vec![],
            run_confirmed: false,
        };
        let active = self.0.store.active().await?;
        let recent = self.activities(Some(&definition.id), 1).await?;
        let reason = if event.origin_automation_id.is_some() {
            Some("a chained trigger caused by an automation action was blocked".into())
        } else if active.iter().any(|a| a.automation_id == definition.id) {
            Some("the previous run has not finished".into())
        } else if active.len() >= 128 {
            Some("the pending automation limit has been reached; handle existing activities and try again".into())
        } else if !manual
            && recent
                .first()
                .is_some_and(|a| (Utc::now() - a.created_at).num_milliseconds() < 1500)
        {
            Some("duplicate events received within a short interval were merged".into())
        } else if !manual {
            condition_failure(
                &definition.conditions,
                &self.0.runner.environment().await,
                event.occurred_at,
            )
        } else {
            None
        };
        if let Some(reason) = reason {
            activity.status = ActivityStatus::Skipped;
            activity.reason = Some(reason);
            activity.finished_at = Some(Utc::now());
        }
        if !self.0.store.insert_activity(&activity).await? {
            return Ok(self
                .activities(Some(&definition.id), 100)
                .await?
                .into_iter()
                .find(|a| a.event.id == event.id)
                .map(|a| a.id)
                .unwrap_or(activity.id));
        }
        if !activity.status.terminal() {
            for (index, step) in definition.steps.iter().enumerate() {
                let action = if let AutomationStep::QuickAction { action_id } = step {
                    match self.0.runner.snapshot_action(action_id).await {
                        Ok(a) => Some(a),
                        Err(error) => {
                            activity.status = ActivityStatus::Failed;
                            activity.reason = Some(error);
                            activity.finished_at = Some(Utc::now());
                            break;
                        }
                    }
                } else {
                    None
                };
                activity.steps.push(StepRun {
                    index,
                    step: step.clone(),
                    action,
                    status: ActivityStatus::Queued,
                    started_at: None,
                    finished_at: None,
                    result: StepResult::default(),
                });
            }
        }
        if !activity.status.terminal() && definition.run_mode == RunMode::AskBeforeRun {
            activity.status = ActivityStatus::AwaitingConfirmation;
            activity.reason = Some("the automation is configured to ask before running".into());
        }
        self.persist(&activity).await?;
        if activity.status == ActivityStatus::Queued {
            self.launch(activity.id.clone()).await;
        }
        self.0.store.prune(false).await?;
        Ok(activity.id)
    }
    async fn launch(&self, id: String) {
        let cancel = CancellationToken::new();
        let ticket = uuid::Uuid::new_v4().to_string();
        self.0
            .active
            .lock()
            .await
            .insert(id.clone(), (ticket.clone(), cancel.clone()));
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.execute(&id, cancel).await {
                tracing::error!(%error,run_id=%id,"automation execution failed");
                if let Ok(mut activity) = engine.activity(&id).await {
                    activity.status = ActivityStatus::Failed;
                    activity.reason = Some(error.to_string());
                    activity.finished_at = Some(Utc::now());
                    let _ = engine.persist(&activity).await;
                }
            }
            let mut active = engine.0.active.lock().await;
            if active
                .get(&id)
                .is_some_and(|(current, _)| current == &ticket)
            {
                active.remove(&id);
            }
        });
    }
    pub async fn confirm(&self, id: &str) -> Result<()> {
        let _guard = self.0.gate.lock().await;
        let mut activity = self.activity(id).await?;
        if activity.status != ActivityStatus::AwaitingConfirmation {
            return Err(AutomationError::Invalid(
                "this activity has already been handled".into(),
            ));
        }
        if self.0.runner.environment().await.locked {
            return Err(AutomationError::Invalid(
                "unlock this computer before confirming the run".into(),
            ));
        }
        if !activity.run_confirmed && activity.definition.run_mode == RunMode::AskBeforeRun {
            activity.run_confirmed = true;
        } else if let Some(step) = activity
            .steps
            .iter()
            .find(|s| s.status == ActivityStatus::AwaitingConfirmation)
        {
            activity.confirmed_steps.push(step.index);
        }
        activity.status = ActivityStatus::Queued;
        activity.reason = None;
        self.persist(&activity).await?;
        self.launch(id.into()).await;
        Ok(())
    }
    pub async fn cancel(&self, id: &str) -> Result<()> {
        let _guard = self.0.gate.lock().await;
        let mut activity = self.activity(id).await?;
        if activity.status.terminal() {
            return Ok(());
        }
        if let Some((_, cancel)) = self.0.active.lock().await.get(id) {
            cancel.cancel();
            if activity.status == ActivityStatus::AwaitingConfirmation {
                activity.status = ActivityStatus::Canceled;
                activity.reason = Some("the user skipped this run".into());
                activity.finished_at = Some(Utc::now());
                self.persist(&activity).await?;
            }
        } else {
            activity.status = ActivityStatus::Canceled;
            activity.reason = Some("the user skipped this run".into());
            activity.finished_at = Some(Utc::now());
            self.persist(&activity).await?;
        }
        Ok(())
    }
    async fn execute(&self, id: &str, cancel: CancellationToken) -> Result<()> {
        let mut activity = self.activity(id).await?;
        let permit =
            tokio::select! {p=self.0.capacity.acquire()=>p.ok(),_=cancel.cancelled()=>None};
        if permit.is_none() {
            activity.status = ActivityStatus::Canceled;
            activity.finished_at = Some(Utc::now());
            return self.persist(&activity).await;
        }
        for index in 0..activity.steps.len() {
            if activity.steps[index].status == ActivityStatus::Succeeded {
                continue;
            }
            if cancel.is_cancelled() {
                activity.status = ActivityStatus::Canceled;
                break;
            }
            let action = activity.steps[index].action.clone();
            if action.as_ref().is_some_and(|a| a.requires_confirmation)
                && !activity.confirmed_steps.contains(&index)
            {
                // Serialize the waiting transition with cancel/confirm. A canceled queued
                // task must not subsequently resurrect itself as a confirmation request.
                let _guard = self.0.gate.lock().await;
                if cancel.is_cancelled() {
                    activity.status = ActivityStatus::Canceled;
                    break;
                }
                activity.status = ActivityStatus::AwaitingConfirmation;
                activity.steps[index].status = ActivityStatus::AwaitingConfirmation;
                activity.reason = Some(format!(
                    "confirm action {}: {}",
                    index + 1,
                    action.as_ref().unwrap().name
                ));
                self.persist(&activity).await?;
                return Ok(());
            }
            activity.status = ActivityStatus::Running;
            activity.reason = None;
            activity.steps[index].status = ActivityStatus::Running;
            activity.steps[index].started_at = Some(Utc::now());
            self.persist(&activity).await?;
            if cancel.is_cancelled() {
                activity.status = ActivityStatus::Canceled;
                break;
            }
            if matches!(
                activity.steps[index].step,
                AutomationStep::QuickAction { .. } | AutomationStep::Shell { .. }
            ) {
                self.0.runner.record_origin(&activity.automation_id).await;
            }
            let result = if action.as_ref().is_some_and(|a| a.requires_unlocked_session)
                && self.0.runner.environment().await.locked
            {
                StepResult {
                    error: Some(
                        "the system is locked; this action requires an unlocked graphical session"
                            .into(),
                    ),
                    ..Default::default()
                }
            } else {
                match &activity.steps[index].step {
                    AutomationStep::QuickAction { .. } => {
                        self.0
                            .runner
                            .execute_action(
                                action.as_ref().ok_or_else(|| {
                                    AutomationError::Invalid(
                                        "quick action snapshot is missing".into(),
                                    )
                                })?,
                                cancel.clone(),
                            )
                            .await
                    }
                    AutomationStep::Shell {
                        shell,
                        script,
                        working_directory,
                        timeout_seconds,
                    } => {
                        run_shell(
                            *shell,
                            script,
                            working_directory.as_deref(),
                            *timeout_seconds,
                            &activity.event,
                            cancel.clone(),
                        )
                        .await
                    }
                    AutomationStep::Delay { duration_seconds } => {
                        tokio::select! { _=cancel.cancelled()=>{}, _=tokio::time::sleep(Duration::from_secs(*duration_seconds))=>{} };
                        StepResult::default()
                    }
                    AutomationStep::Notification {
                        title,
                        body,
                        send_to_connected_devices,
                    } => match (
                        render_text(title, &activity.event),
                        render_text(body, &activity.event),
                    ) {
                        (Ok(title), Ok(body)) => {
                            self.0
                                .runner
                                .notify(&title, &body, *send_to_connected_devices)
                                .await
                        }
                        (Err(e), _) | (_, Err(e)) => StepResult {
                            error: Some(e.to_string()),
                            ..Default::default()
                        },
                    },
                }
            };
            let status = if cancel.is_cancelled() {
                ActivityStatus::Canceled
            } else if result.error.is_some() {
                ActivityStatus::Failed
            } else {
                ActivityStatus::Succeeded
            };
            activity.steps[index].result = result.bounded();
            activity.steps[index].status = status;
            activity.steps[index].finished_at = Some(Utc::now());
            if status != ActivityStatus::Succeeded {
                activity.status = status;
                activity.reason = activity.steps[index].result.error.clone();
                break;
            }
            self.persist(&activity).await?;
        }
        if activity.status == ActivityStatus::Running || activity.status == ActivityStatus::Queued {
            activity.status = ActivityStatus::Succeeded;
        }
        for step in &mut activity.steps {
            if matches!(
                step.status,
                ActivityStatus::Queued | ActivityStatus::Running
            ) {
                step.status = if activity.status == ActivityStatus::Canceled {
                    ActivityStatus::Canceled
                } else {
                    ActivityStatus::Skipped
                };
                step.result.error = Some(
                    if activity.status == ActivityStatus::Canceled {
                        "run canceled"
                    } else {
                        "subsequent actions were not run because the previous action did not succeed"
                    }
                    .into(),
                );
                step.finished_at = Some(Utc::now());
            }
        }
        activity.finished_at = Some(Utc::now());
        self.persist(&activity).await
    }
    pub async fn recover(&self) -> Result<usize> {
        let _guard = self.0.gate.lock().await;
        let mut count = 0;
        for mut activity in self.0.store.active().await? {
            if activity.status == ActivityStatus::AwaitingConfirmation {
                continue;
            }
            activity.status = ActivityStatus::Interrupted;
            activity.reason =
                Some("ArcRelay restarted and interrupted the previous run; completed actions will not run again automatically".into());
            activity.finished_at = Some(Utc::now());
            for step in &mut activity.steps {
                if step.status == ActivityStatus::Running {
                    step.status = ActivityStatus::Interrupted;
                    step.finished_at = Some(Utc::now());
                }
            }
            self.persist(&activity).await?;
            count += 1;
        }
        Ok(count)
    }
    pub async fn tick(&self) -> Result<()> {
        let _guard = self.0.gate.lock().await;
        for (id, at) in self.0.store.due().await? {
            let definition = self.get(&id).await?;
            if !definition.enabled {
                continue;
            }
            let now = Utc::now();
            let Some(planned) = Utc.timestamp_opt(at, 0).single() else {
                continue;
            };
            let catch_up = matches!(
                definition.trigger,
                AutomationTrigger::Schedule { catch_up: true, .. }
            );
            let mut event = AutomationEvent::new("schedule");
            event.id = format!("schedule:{}:{}:{}", id, definition.revision, at);
            event
                .variables
                .insert("event.schedule.plannedAt".into(), planned.to_rfc3339());
            event
                .variables
                .insert("event.schedule.actualAt".into(), now.to_rfc3339());
            if (now - planned).num_seconds() <= 90 || catch_up {
                self.enqueue(definition.clone(), event, false).await?;
            } else {
                let activity = AutomationActivity {
                    id: uuid::Uuid::new_v4().to_string(),
                    automation_id: id.clone(),
                    definition: definition.clone(),
                    event,
                    status: ActivityStatus::Skipped,
                    reason: Some(
                        "the scheduled time was missed while the computer was asleep or offline"
                            .into(),
                    ),
                    created_at: now,
                    finished_at: Some(now),
                    steps: vec![],
                    confirmed_steps: vec![],
                    run_confirmed: false,
                };
                if self.0.store.insert_activity(&activity).await? {
                    self.persist(&activity).await?;
                }
            }
            if let Some(next) = next_schedule(&definition.trigger, now) {
                self.0.store.advance_schedule(&id, next.timestamp()).await?;
            }
        }
        Ok(())
    }
    pub fn start_scheduler(&self) -> tokio::task::JoinHandle<()> {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            loop {
                interval.tick().await;
                if let Err(error) = engine.tick().await {
                    tracing::warn!(%error,"automation schedule failed");
                }
            }
        })
    }
}
