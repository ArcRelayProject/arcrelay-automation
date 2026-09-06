use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use ts_rs::TS;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationDefinition {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub trigger: AutomationTrigger,
    pub conditions: Vec<AutomationCondition>,
    pub steps: Vec<AutomationStep>,
    pub run_mode: RunMode,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub legacy_id: Option<String>,
}

impl AutomationDefinition {
    pub fn summary(&self) -> String {
        self.summary_with_actions(&BTreeMap::new())
    }
    pub fn summary_with_actions(&self, names: &BTreeMap<String, String>) -> String {
        let trigger = match &self.trigger {
            AutomationTrigger::Manual => "Run manually".into(),
            AutomationTrigger::Schedule { time, weekdays, .. } => format!(
                "Every {} at {time}",
                weekdays
                    .iter()
                    .map(|d| ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                        .get(*d as usize)
                        .copied()
                        .unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("、")
            ),
            AutomationTrigger::Application { event, apps } => format!(
                "When {} {}",
                apps.iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" or "),
                match event {
                    ApplicationEvent::Started => "starts",
                    ApplicationEvent::Exited => "exits",
                    ApplicationEvent::Foreground => "moves to the foreground",
                    ApplicationEvent::Background => "moves to the background",
                }
            ),
            AutomationTrigger::System { event } => match event {
                SessionEvent::Locked => "When the system locks",
                SessionEvent::Unlocked => "When the system unlocks",
                SessionEvent::Sleeping => "When the system goes to sleep",
                SessionEvent::Resumed => "When the system resumes from sleep",
                SessionEvent::Started => "When ArcRelay starts",
            }
            .into(),
            AutomationTrigger::Device { connected, .. } => if *connected {
                "When a device connects"
            } else {
                "When a device disconnects"
            }
            .into(),
            AutomationTrigger::Transfer { received, .. } => if *received {
                "When receiving files completes"
            } else {
                "When sending files completes"
            }
            .into(),
            AutomationTrigger::Hotkey { shortcut } => format!("When {shortcut} is pressed"),
        };
        let conditions = self
            .conditions
            .iter()
            .map(|c| match c {
                AutomationCondition::TimeRange {
                    weekdays,
                    start,
                    end,
                    ..
                } => format!(
                    "{} {start}–{end}",
                    weekdays
                        .iter()
                        .map(|d| ["", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                            .get(*d as usize)
                            .copied()
                            .unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("、")
                ),
                AutomationCondition::ApplicationRunning { app, running } => format!(
                    "{}{}",
                    app.name,
                    if *running {
                        "is running"
                    } else {
                        "is not running"
                    }
                ),
                AutomationCondition::DeviceConnected { connected, .. } => {
                    format!(
                        "the selected device {}",
                        if *connected {
                            "is connected"
                        } else {
                            "is not connected"
                        }
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(", and ");
        let steps = self
            .steps
            .iter()
            .map(|s| match s {
                AutomationStep::QuickAction { action_id } => names
                    .get(action_id)
                    .cloned()
                    .unwrap_or_else(|| "Quick action".into()),
                AutomationStep::Shell { .. } => "Run script".into(),
                AutomationStep::Delay { duration_seconds } => {
                    format!("Wait {duration_seconds} seconds")
                }
                AutomationStep::Notification { title, .. } => {
                    format!("Show notification \"{title}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" → ");
        format!(
            "{trigger}{}, run {steps}",
            if conditions.is_empty() {
                String::new()
            } else {
                format!(", only when {conditions}")
            }
        )
    }
    pub fn new(trigger: AutomationTrigger) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            enabled: true,
            trigger,
            conditions: vec![],
            steps: vec![],
            run_mode: RunMode::Automatic,
            revision: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            legacy_id: None,
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationIdentity {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationTrigger {
    Manual,
    Schedule {
        time: String,
        weekdays: Vec<u8>,
        timezone: String,
        catch_up: bool,
    },
    Application {
        event: ApplicationEvent,
        apps: Vec<ApplicationIdentity>,
    },
    System {
        event: SessionEvent,
    },
    Device {
        connected: bool,
        device_ids: Vec<String>,
    },
    Transfer {
        received: bool,
        device_ids: Vec<String>,
        file_kinds: Vec<String>,
    },
    Hotkey {
        shortcut: String,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub enum ApplicationEvent {
    Started,
    Exited,
    Foreground,
    Background,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub enum SessionEvent {
    Locked,
    Unlocked,
    Sleeping,
    Resumed,
    Started,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub enum RunMode {
    Automatic,
    AskBeforeRun,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub enum ShellKind {
    System,
    Sh,
    Bash,
    Zsh,
    Powershell,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationCondition {
    TimeRange {
        weekdays: Vec<u8>,
        start: String,
        end: String,
        timezone: String,
    },
    ApplicationRunning {
        app: ApplicationIdentity,
        running: bool,
    },
    DeviceConnected {
        device_id: String,
        connected: bool,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AutomationStep {
    QuickAction {
        action_id: String,
    },
    Shell {
        shell: ShellKind,
        script: String,
        working_directory: Option<String>,
        timeout_seconds: u32,
    },
    Delay {
        duration_seconds: u64,
    },
    Notification {
        title: String,
        body: String,
        send_to_connected_devices: bool,
    },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationEvent {
    pub id: String,
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub variables: BTreeMap<String, String>,
    pub origin_automation_id: Option<String>,
}
impl AutomationEvent {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            occurred_at: Utc::now(),
            variables: BTreeMap::new(),
            origin_automation_id: None,
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "camelCase")]
pub enum ActivityStatus {
    AwaitingConfirmation,
    Skipped,
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
    Interrupted,
}
impl ActivityStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Skipped | Self::Succeeded | Self::Failed | Self::Canceled | Self::Interrupted
        )
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::AwaitingConfirmation => "awaitingConfirmation",
            Self::Skipped => "skipped",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ActionSnapshot {
    pub name: String,
    pub definition: serde_json::Value,
    pub requires_confirmation: bool,
    pub requires_unlocked_session: bool,
    #[serde(default)]
    pub requirements: Vec<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StepRun {
    pub index: usize,
    pub step: AutomationStep,
    pub action: Option<ActionSnapshot>,
    pub status: ActivityStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub result: StepResult,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}
impl StepResult {
    /// Bound persisted output in bytes, including lossy UTF-8 decoding and native action results.
    pub fn bounded(mut self) -> Self {
        fn truncate(text: &mut String) {
            let mut end = text.len().min(64 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        truncate(&mut self.stdout);
        truncate(&mut self.stderr);
        if let Some(error) = &mut self.error {
            truncate(error);
        }
        self
    }
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationActivity {
    pub id: String,
    pub automation_id: String,
    pub definition: AutomationDefinition,
    pub event: AutomationEvent,
    pub status: ActivityStatus,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub steps: Vec<StepRun>,
    pub confirmed_steps: Vec<usize>,
    pub run_confirmed: bool,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub id: String,
    pub available: bool,
    pub reason: Option<String>,
    pub remedy: Option<String>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AutomationIssue {
    pub code: String,
    pub message: String,
    pub remedy: String,
    pub step_index: Option<usize>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default)]
pub struct EnvironmentState {
    pub running_app_ids: Vec<String>,
    pub connected_device_ids: Vec<String>,
    pub locked: bool,
}

impl AutomationTrigger {
    pub fn capability(&self) -> String {
        match self {
            Self::Manual => "manual".into(),
            Self::Schedule { .. } => "schedule".into(),
            Self::Application { event, .. } => format!(
                "application.{}",
                match event {
                    ApplicationEvent::Started => "started",
                    ApplicationEvent::Exited => "exited",
                    ApplicationEvent::Foreground => "foreground",
                    ApplicationEvent::Background => "background",
                }
            ),
            Self::System { event } => format!(
                "system.{}",
                match event {
                    SessionEvent::Locked => "locked",
                    SessionEvent::Unlocked => "unlocked",
                    SessionEvent::Sleeping => "sleeping",
                    SessionEvent::Resumed => "resumed",
                    SessionEvent::Started => "started",
                }
            ),
            Self::Device { connected, .. } => format!(
                "device.{}",
                if *connected {
                    "connected"
                } else {
                    "disconnected"
                }
            ),
            Self::Transfer { received, .. } => {
                format!("transfer.{}", if *received { "received" } else { "sent" })
            }
            Self::Hotkey { .. } => "hotkey".into(),
        }
    }
    pub fn matches(&self, event: &AutomationEvent) -> bool {
        if self.capability() != event.kind {
            return false;
        }
        let get = |key: &str| {
            event
                .variables
                .get(key)
                .map(String::as_str)
                .unwrap_or_default()
        };
        match self {
            Self::Application { apps, .. } => apps.iter().any(|app| app.id == get("event.app.id")),
            Self::Device { device_ids, .. } => {
                device_ids.is_empty() || device_ids.iter().any(|id| id == get("event.device.id"))
            }
            Self::Transfer {
                device_ids,
                file_kinds,
                ..
            } => {
                (device_ids.is_empty() || device_ids.iter().any(|id| id == get("event.device.id")))
                    && (file_kinds.is_empty()
                        || file_kinds
                            .iter()
                            .any(|kind| get("event.file.kinds").split(',').any(|v| v == kind)))
            }
            Self::Hotkey { shortcut } => shortcut == get("event.shortcut"),
            Self::Schedule { .. } => false, // Scheduler targets a single automation with a durable time-slot ID.
            _ => true,
        }
    }
}
