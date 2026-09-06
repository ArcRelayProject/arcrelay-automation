use crate::*;
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

pub fn validate(definition: &AutomationDefinition) -> Result<()> {
    let invalid = |message: &str| AutomationError::Invalid(message.into());
    if definition.id.is_empty() || definition.id.len() > 128 || definition.name.len() > 256 {
        return Err(invalid("name or identifier is too long"));
    }
    if definition.steps.is_empty()
        || definition.steps.len() > 64
        || definition.conditions.len() > 16
    {
        return Err(invalid("add 1–64 actions and no more than 16 conditions"));
    }
    if serde_json::to_vec(definition)?.len() > 512 * 1024 {
        return Err(invalid("configuration cannot exceed 512 KiB"));
    }
    match &definition.trigger {
        AutomationTrigger::Schedule {
            time,
            weekdays,
            timezone,
            ..
        } => validate_time(time, weekdays, timezone)?,
        AutomationTrigger::Application { apps, .. } => {
            if apps.is_empty()
                || apps.len() > 64
                || apps.iter().any(|a| a.id.is_empty() || a.path.is_empty())
            {
                return Err(invalid(
                    "select an application from the installed applications",
                ));
            }
        }
        AutomationTrigger::Hotkey { shortcut } if shortcut.trim().is_empty() => {
            return Err(invalid("record a global shortcut"))
        }
        _ => {}
    }
    for condition in &definition.conditions {
        match condition {
            AutomationCondition::TimeRange {
                start,
                end,
                weekdays,
                timezone,
            } => {
                validate_time(start, weekdays, timezone)?;
                validate_time(end, weekdays, timezone)?;
            }
            AutomationCondition::ApplicationRunning { app, .. } if app.id.is_empty() => {
                return Err(invalid("select an application for the condition"))
            }
            AutomationCondition::DeviceConnected { device_id, .. } if device_id.is_empty() => {
                return Err(invalid("select a device for the condition"))
            }
            _ => {}
        }
    }
    for step in &definition.steps {
        match step {
            AutomationStep::QuickAction { action_id } if action_id.is_empty() => {
                return Err(invalid("select a quick action"))
            }
            AutomationStep::Shell {
                script,
                timeout_seconds,
                ..
            } => {
                if script.trim().is_empty()
                    || script.len() > 65536
                    || !(1..=3600).contains(timeout_seconds)
                {
                    return Err(invalid("the script cannot be empty or exceed 64 KiB, and the timeout must be 1–3600 seconds"));
                }
                validate_variables(script, &definition.trigger)?;
            }
            AutomationStep::Delay { duration_seconds } if *duration_seconds > 86400 => {
                return Err(invalid("delay cannot exceed 24 hours"))
            }
            AutomationStep::Notification { title, body, .. } => {
                if title.trim().is_empty() || title.len() > 256 || body.len() > 8192 {
                    return Err(invalid(
                        "enter a notification title; the body cannot exceed 8 KiB",
                    ));
                }
                validate_variables(title, &definition.trigger)?;
                validate_variables(body, &definition.trigger)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_time(time: &str, weekdays: &[u8], timezone: &str) -> Result<()> {
    if NaiveTime::parse_from_str(time, "%H:%M").is_err()
        || weekdays.is_empty()
        || weekdays.iter().any(|d| !(1..=7).contains(d))
        || timezone.parse::<Tz>().is_err()
    {
        return Err(AutomationError::Invalid(
            "select a valid time, weekday, and time zone".into(),
        ));
    }
    Ok(())
}

pub fn available_variables(trigger: &AutomationTrigger) -> Vec<&'static str> {
    match trigger {
        AutomationTrigger::Application { .. } => {
            vec!["event.app.name", "event.app.path", "event.app.id"]
        }
        AutomationTrigger::Device { .. } => vec!["event.device.name", "event.device.id"],
        AutomationTrigger::Transfer { received: true, .. } => vec![
            "event.device.name",
            "event.device.id",
            "event.file.directory",
            "event.file.count",
            "event.file.firstPath",
        ],
        AutomationTrigger::Transfer {
            received: false, ..
        } => vec!["event.device.name", "event.device.id", "event.file.count"],
        AutomationTrigger::Schedule { .. } => {
            vec!["event.schedule.plannedAt", "event.schedule.actualAt"]
        }
        _ => vec![],
    }
}
fn validate_variables(text: &str, trigger: &AutomationTrigger) -> Result<()> {
    let allowed = available_variables(trigger);
    let mut rest = text;
    while let Some((_, tail)) = rest.split_once("{{") {
        let (key, next) = tail.split_once("}}").ok_or_else(|| {
            AutomationError::Invalid("event variable is missing a closing tag".into())
        })?;
        if !allowed.contains(&key.trim()) {
            return Err(AutomationError::Invalid(format!(
                "this trigger does not provide variable {}",
                key.trim()
            )));
        }
        rest = next;
    }
    Ok(())
}
pub fn render_text(text: &str, event: &AutomationEvent) -> Result<String> {
    let mut output = String::new();
    let mut rest = text;
    while let Some((head, tail)) = rest.split_once("{{") {
        let (key, next) = tail
            .split_once("}}")
            .ok_or_else(|| AutomationError::Invalid("invalid variable".into()))?;
        output.push_str(head);
        output.push_str(event.variables.get(key.trim()).ok_or_else(|| {
            AutomationError::Invalid(format!(
                "the event does not provide variable {}; test with a real event",
                key.trim()
            ))
        })?);
        rest = next;
    }
    output.push_str(rest);
    Ok(output)
}

pub fn condition_failure(
    conditions: &[AutomationCondition],
    state: &EnvironmentState,
    now: DateTime<Utc>,
) -> Option<String> {
    for condition in conditions {
        let (matches, reason) = match condition {
            AutomationCondition::ApplicationRunning { app, running } => (
                state.running_app_ids.contains(&app.id) == *running,
                format!(
                    "the running state of application {} does not satisfy the condition",
                    app.name
                ),
            ),
            AutomationCondition::DeviceConnected {
                device_id,
                connected,
            } => (
                state.connected_device_ids.contains(device_id) == *connected,
                "the device connection state does not satisfy the condition".into(),
            ),
            AutomationCondition::TimeRange {
                start,
                end,
                weekdays,
                timezone,
            } => {
                let matches = (|| {
                    let local = now.with_timezone(&timezone.parse::<Tz>().ok()?);
                    let start = NaiveTime::parse_from_str(start, "%H:%M").ok()?;
                    let end = NaiveTime::parse_from_str(end, "%H:%M").ok()?;
                    let time = local.time();
                    // An overnight interval belongs to the weekday on which it starts.
                    let day = if start > end && time < end {
                        (local - Duration::days(1)).weekday()
                    } else {
                        local.weekday()
                    };
                    Some(
                        weekdays.contains(&(day.number_from_monday() as u8))
                            && if start < end {
                                time >= start && time < end
                            } else if start > end {
                                time >= start || time < end
                            } else {
                                true
                            },
                    )
                })()
                .unwrap_or(false);
                (
                    matches,
                    "the current time is outside the configured schedule".into(),
                )
            }
        };
        if !matches {
            return Some(reason);
        }
    }
    None
}

/// Choose the first occurrence during a DST overlap; nonexistent local times are skipped.
pub fn next_schedule(trigger: &AutomationTrigger, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let AutomationTrigger::Schedule {
        time,
        weekdays,
        timezone,
        ..
    } = trigger
    else {
        return None;
    };
    let tz = timezone.parse::<Tz>().ok()?;
    let time = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
    let start = after.with_timezone(&tz).date_naive();
    for offset in 0..=8 {
        let date = start.checked_add_signed(Duration::days(offset))?;
        if !weekdays.contains(&(date.weekday().number_from_monday() as u8)) {
            continue;
        }
        if let Some(candidate) = tz.from_local_datetime(&date.and_time(time)).earliest() {
            let candidate = candidate.with_timezone(&Utc);
            if candidate > after {
                return Some(candidate);
            }
        }
    }
    None
}
