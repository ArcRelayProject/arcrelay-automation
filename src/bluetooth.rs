//! Radio-independent proximity policy. No OS handles, timers, or runtime required.
use crate::{AutomationError, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const BLUETOOTH_STALE_MS: u64 = 5_000;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BluetoothCalibration {
    pub near_rssi: i16,
    pub far_rssi: i16,
}
impl BluetoothCalibration {
    pub fn validate(&self) -> Result<()> {
        if !(-110..=-20).contains(&self.near_rssi)
            || !(-110..=-20).contains(&self.far_rssi)
            || self.near_rssi - self.far_rssi < 10
        {
            return Err(AutomationError::Invalid("Bluetooth calibration needs two stable positions at least 10 dBm apart; recalibrate the device".into()));
        }
        Ok(())
    }
    pub fn away_boundary(&self) -> i16 {
        self.far_rssi + 3
    }
    pub fn return_boundary(&self) -> i16 {
        self.near_rssi - 3
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BluetoothTrigger {
    pub device_id: String,
    pub device_name: String,
    pub calibration: Option<BluetoothCalibration>,
    pub duration_seconds: u32,
    /// Explicit opt-in. Loss of advertisements is not proof of departure.
    pub signal_loss_seconds: Option<u32>,
}
impl BluetoothTrigger {
    pub fn validate(&self) -> Result<()> {
        if self.device_id.is_empty() || self.device_id.len() > 512 || self.device_name.len() > 256 {
            return Err(AutomationError::Invalid("select a Bluetooth device".into()));
        }
        if !(5..=300).contains(&self.duration_seconds)
            || self
                .signal_loss_seconds
                .is_some_and(|v| !(15..=300).contains(&v))
        {
            return Err(AutomationError::Invalid("Bluetooth duration must be 5–300 seconds; signal-loss timeout must be 15–300 seconds".into()));
        }
        self.calibration
            .as_ref()
            .ok_or_else(|| {
                AutomationError::Invalid(
                    "calibrate the Bluetooth device before enabling this automation".into(),
                )
            })?
            .validate()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BluetoothReading {
    pub rssi: i16,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct BluetoothDetection {
    pub state: String,
    pub elapsed_seconds: u32,
    pub fired: bool,
}

#[derive(Default)]
pub struct BluetoothDetector {
    armed: bool,
    near_since: Option<u64>,
    far_since: Option<u64>,
    last_tick: Option<u64>,
    fired: bool,
}
impl BluetoothDetector {
    pub fn update(
        &mut self,
        config: &BluetoothTrigger,
        now: u64,
        reading: Option<BluetoothReading>,
        healthy: bool,
    ) -> BluetoothDetection {
        let gap = self
            .last_tick
            .is_some_and(|at| now < at || now - at > BLUETOOTH_STALE_MS);
        if gap || !healthy || config.validate().is_err() {
            *self = Self::default();
        }
        self.last_tick = Some(now);
        let result = |state: &str, elapsed_seconds, fired| BluetoothDetection {
            state: state.into(),
            elapsed_seconds,
            fired,
        };
        if !healthy || config.validate().is_err() {
            return result("unavailable", 0, false);
        }
        let calibration = config.calibration.as_ref().unwrap();
        let fresh = reading.filter(|r| {
            r.at_ms <= now && now - r.at_ms <= BLUETOOTH_STALE_MS && (-110..=-20).contains(&r.rssi)
        });
        let Some(reading) = fresh else {
            self.near_since = None;
            self.far_since = None;
            if self.armed {
                if let (Some(limit), Some(last)) = (config.signal_loss_seconds, reading) {
                    if last.at_ms <= now && now - last.at_ms >= u64::from(limit) * 1000 {
                        self.armed = false;
                        self.fired = true;
                        return result("signalLost", limit, true);
                    }
                }
            }
            return result("noSignal", 0, false);
        };
        if reading.rssi >= calibration.return_boundary() {
            self.far_since = None;
            let since = *self.near_since.get_or_insert(now);
            if now - since >= 3_000 {
                self.armed = true;
                self.fired = false;
            }
            return result(if self.armed { "near" } else { "waitingNear" }, 0, false);
        }
        self.near_since = None;
        if !self.armed {
            return result(
                if self.fired {
                    "triggered"
                } else {
                    "waitingNear"
                },
                0,
                false,
            );
        }
        if reading.rssi > calibration.away_boundary() {
            self.far_since = None;
            return result("near", 0, false);
        }
        let since = *self.far_since.get_or_insert(now);
        let elapsed = ((now - since) / 1000) as u32;
        if elapsed >= config.duration_seconds {
            self.armed = false;
            self.fired = true;
            self.far_since = None;
            return result("far", elapsed, true);
        }
        result("leaving", elapsed, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> BluetoothTrigger {
        BluetoothTrigger {
            device_id: "device".into(),
            device_name: "phone".into(),
            calibration: Some(BluetoothCalibration {
                near_rssi: -55,
                far_rssi: -80,
            }),
            duration_seconds: 5,
            signal_loss_seconds: None,
        }
    }
    fn sample(
        d: &mut BluetoothDetector,
        c: &BluetoothTrigger,
        seconds: u64,
        rssi: i16,
    ) -> BluetoothDetection {
        d.update(
            c,
            seconds * 1000,
            Some(BluetoothReading {
                rssi,
                at_ms: seconds * 1000,
            }),
            true,
        )
    }
    #[test]
    fn departure_requires_near_and_fires_once() {
        let c = config();
        let mut d = BluetoothDetector::default();
        for t in 0..10 {
            assert!(!sample(&mut d, &c, t, -85).fired);
        }
        for t in 10..14 {
            sample(&mut d, &c, t, -55);
        }
        for t in 14..19 {
            assert!(!sample(&mut d, &c, t, -85).fired);
        }
        assert!(sample(&mut d, &c, 19, -85).fired);
        for t in 20..35 {
            assert!(!sample(&mut d, &c, t, -85).fired);
        }
        for t in 35..39 {
            sample(&mut d, &c, t, -55);
        }
        for t in 39..44 {
            sample(&mut d, &c, t, -85);
        }
        assert!(sample(&mut d, &c, 44, -85).fired);
    }
    #[test]
    fn recovery_cancels_countdown_and_suspend_requires_rearming() {
        let c = config();
        let mut d = BluetoothDetector::default();
        for t in 0..4 {
            sample(&mut d, &c, t, -55);
        }
        for t in 4..8 {
            sample(&mut d, &c, t, -85);
        }
        sample(&mut d, &c, 8, -70);
        assert_eq!(sample(&mut d, &c, 9, -85).elapsed_seconds, 0);
        assert_eq!(sample(&mut d, &c, 100, -85).state, "waitingNear");
    }
    #[test]
    fn stale_samples_do_not_accumulate_departure_and_radio_failure_never_fires() {
        let mut c = config();
        let mut d = BluetoothDetector::default();
        for t in 0..4 {
            sample(&mut d, &c, t, -55);
        }
        let old = Some(BluetoothReading {
            rssi: -55,
            at_ms: 3000,
        });
        for t in 4..40 {
            assert!(!d.update(&c, t * 1000, old, true).fired);
        }
        c.signal_loss_seconds = Some(15);
        assert_eq!(d.update(&c, 40000, old, false).state, "unavailable");
        for t in 41..46 {
            assert!(!sample(&mut d, &c, t, -85).fired);
        }
    }
    #[test]
    fn signal_loss_is_explicit_and_only_after_arming() {
        let mut c = config();
        c.signal_loss_seconds = Some(15);
        let mut d = BluetoothDetector::default();
        for t in 0..4 {
            sample(&mut d, &c, t, -55);
        }
        let old = Some(BluetoothReading {
            rssi: -55,
            at_ms: 3000,
        });
        for t in 4..18 {
            assert!(!d.update(&c, t * 1000, old, true).fired);
        }
        let result = d.update(&c, 18000, old, true);
        assert!(result.fired);
        assert_eq!(result.state, "signalLost");
        assert!(!d.update(&c, 19000, old, true).fired);
    }
    #[test]
    fn invalid_calibration_is_rejected_without_runtime() {
        let mut c = config();
        c.calibration.as_mut().unwrap().far_rssi = -60;
        assert!(c.validate().is_err());
        assert_eq!(
            BluetoothDetector::default().update(&c, 0, None, true).state,
            "unavailable"
        );
    }
}
