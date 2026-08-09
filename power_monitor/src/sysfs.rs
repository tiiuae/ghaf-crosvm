// Copyright 2026 TII (SSRC)
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Linux power-supply monitor backed by `/sys/class/power_supply`.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use base::AsRawDescriptor;
use base::RawDescriptor;
use base::ReadNotifier;
use base::Timer;
use base::TimerTrait;

use crate::BatteryData;
use crate::BatteryHealth;
use crate::BatteryStatus;
use crate::PowerClient;
use crate::PowerData;
use crate::PowerMonitor;

const POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn read_string(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn read_u32(path: &Path) -> u32 {
    read_string(path)
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn read_status(path: &Path) -> BatteryStatus {
    match read_string(path).as_deref() {
        Some("Charging") => BatteryStatus::Charging,
        Some("Discharging") => BatteryStatus::Discharging,
        Some("Full" | "Not charging") => BatteryStatus::NotCharging,
        _ => BatteryStatus::Unknown,
    }
}

fn read_health(path: &Path) -> BatteryHealth {
    match read_string(path).as_deref() {
        Some("Good") => BatteryHealth::Good,
        Some("Overheat") => BatteryHealth::Overheat,
        Some("Dead") => BatteryHealth::Dead,
        Some("Over voltage") => BatteryHealth::OverVoltage,
        Some("Unspecified failure") => BatteryHealth::UnspecifiedFailure,
        Some("Cold") => BatteryHealth::Cold,
        Some("Watchdog timer expire") => BatteryHealth::WatchdogTimerExpire,
        Some("Safety timer expire") => BatteryHealth::SafetyTimerExpire,
        Some("Over current") => BatteryHealth::OverCurrent,
        Some("Calibration required") => BatteryHealth::CalibrationRequired,
        Some("Warm") => BatteryHealth::Warm,
        Some("Cool") => BatteryHealth::Cool,
        Some("Hot") => BatteryHealth::Hot,
        Some("No battery") => BatteryHealth::NoBattery,
        _ => BatteryHealth::Unknown,
    }
}

fn read_power_data(root: &Path) -> std::io::Result<PowerData> {
    let mut ac_online = false;
    let mut battery = None;

    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        match read_string(&path.join("type")).as_deref() {
            Some("Battery") if battery.is_none() => {
                if read_string(&path.join("present")).as_deref() == Some("0") {
                    continue;
                }
                battery = Some(BatteryData {
                    status: read_status(&path.join("status")),
                    health: read_health(&path.join("health")),
                    percent: read_u32(&path.join("capacity")).min(100),
                    voltage: read_u32(&path.join("voltage_now")),
                    current: read_u32(&path.join("current_now")),
                    charge_counter: read_u32(&path.join("charge_now")),
                    charge_full: read_u32(&path.join("charge_full")),
                });
            }
            Some("Mains" | "USB" | "USB_C" | "USB_DCP" | "USB_CDP" | "USB_ACA") => {
                ac_online |= read_string(&path.join("online")).as_deref() == Some("1");
            }
            _ => {}
        }
    }

    Ok(PowerData { ac_online, battery })
}

/// Periodically samples Linux power-supply state and notifies the Goldfish device.
pub struct SysfsMonitor {
    root: PathBuf,
    timer: Timer,
    last_data: Option<PowerData>,
}

impl SysfsMonitor {
    /// Connects a monitor to the host power-supply class.
    pub fn connect() -> Result<Box<dyn PowerMonitor>, Box<dyn Error>> {
        Self::connect_at(PathBuf::from(POWER_SUPPLY_PATH))
            .map(|monitor| Box::new(monitor) as Box<dyn PowerMonitor>)
            .map_err(Into::into)
    }

    fn connect_at(root: PathBuf) -> std::io::Result<Self> {
        let mut timer = Timer::new().map_err(std::io::Error::from)?;
        timer
            .reset_repeating(POLL_INTERVAL)
            .map_err(std::io::Error::from)?;
        let last_data = read_power_data(&root).ok();
        Ok(Self {
            root,
            timer,
            last_data,
        })
    }
}

impl PowerMonitor for SysfsMonitor {
    fn read_message(&mut self) -> Result<Option<PowerData>, Box<dyn Error>> {
        self.timer.mark_waited()?;
        let data = read_power_data(&self.root)?;
        if self.last_data.as_ref() == Some(&data) {
            return Ok(None);
        }
        self.last_data = Some(data.clone());
        Ok(Some(data))
    }
}

impl AsRawDescriptor for SysfsMonitor {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.timer.as_raw_descriptor()
    }
}

impl ReadNotifier for SysfsMonitor {
    fn get_read_notifier(&self) -> &dyn AsRawDescriptor {
        self
    }
}

/// One-shot client used to initialize Goldfish battery state before the first poll.
pub struct SysfsClient {
    root: PathBuf,
    last_request_timestamp: Option<SystemTime>,
}

impl SysfsClient {
    /// Connects a client to the host power-supply class.
    pub fn connect() -> Result<Box<dyn PowerClient>, Box<dyn Error>> {
        Ok(Box::new(Self {
            root: PathBuf::from(POWER_SUPPLY_PATH),
            last_request_timestamp: None,
        }))
    }
}

impl PowerClient for SysfsClient {
    fn get_power_data(&mut self) -> Result<PowerData, Box<dyn Error>> {
        self.last_request_timestamp = Some(SystemTime::now());
        Ok(read_power_data(&self.root)?)
    }

    fn last_request_timestamp(&self) -> Option<SystemTime> {
        self.last_request_timestamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_property(root: &Path, supply: &str, property: &str, value: &str) {
        let supply = root.join(supply);
        fs::create_dir_all(&supply).unwrap();
        fs::write(supply.join(property), value).unwrap();
    }

    #[test]
    fn reads_battery_and_ac_state() {
        let root = tempfile::tempdir().unwrap();
        write_property(root.path(), "BAT0", "type", "Battery\n");
        write_property(root.path(), "BAT0", "present", "1\n");
        write_property(root.path(), "BAT0", "capacity", "73\n");
        write_property(root.path(), "BAT0", "status", "Discharging\n");
        write_property(root.path(), "BAT0", "health", "Good\n");
        write_property(root.path(), "BAT0", "voltage_now", "12000000\n");
        write_property(root.path(), "BAT0", "current_now", "900000\n");
        write_property(root.path(), "BAT0", "charge_now", "3000000\n");
        write_property(root.path(), "BAT0", "charge_full", "4100000\n");
        write_property(root.path(), "AC", "type", "Mains\n");
        write_property(root.path(), "AC", "online", "0\n");

        let data = read_power_data(root.path()).unwrap();
        assert!(!data.ac_online);
        let battery = data.battery.unwrap();
        assert_eq!(battery.percent, 73);
        assert_eq!(battery.status, BatteryStatus::Discharging);
        assert_eq!(battery.health, BatteryHealth::Good);
        assert_eq!(battery.voltage, 12_000_000);
    }

    #[test]
    fn reports_no_battery_on_tower() {
        let root = tempfile::tempdir().unwrap();
        write_property(root.path(), "AC", "type", "Mains\n");
        write_property(root.path(), "AC", "online", "1\n");

        let data = read_power_data(root.path()).unwrap();
        assert!(data.ac_online);
        assert!(data.battery.is_none());
    }
}
