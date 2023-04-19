mod sensors;

use self::sensors::Sensors;
use serde::Deserialize;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::process::Command;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

#[derive(Deserialize)]
struct Config {
    request_shutdown_battery_percent: Option<f64>,
    force_shutdown_timeout_secs: Option<f64>,
}

const DEVICEPATH: &str = if let Ok(product_name) = std::process::Command::new("cat")
        .arg("/sys/devices/virtual/dmi/id/product_name")
        .output()
        .map(|o| o.stdout)
        .map(String::from_utf8)
        .map_err(|e| format!("Failed to decode command output: {}", e)) // <--- new line
        .and_then(|opt| opt.ok_or_else(|| "Invalid utf-8 in command output".to_owned()))
    {
        if product_name.trim() == "Jupiter 1" {
            "/sys/class/power_supply/BAT1/"
        } else {
            "/sys/class/power_supply/BAT*/"
        }
    } else {
        "/sys/class/power_supply/BAT*/"
    };

fn read_battery_string(var_name: &str) -> Option<String> {
    let path = format!("{}/{}", DEVICEPATH, var_name);
    match fs::read_to_string(&path) {
        Err(err) => {
            eprintln!("read {path}: {err}");
            None
        }
        Ok(string) => Some(string.trim().to_owned()),
    }
}

fn read_battery_f64(var_name: &str) -> Option<f64> {
    let path = format!("{}/{}", DEVICEPATH, var_name);
    match fs::read_to_string(&path) {
        Err(err) => {
            eprintln!("read {path}: {err}");
            None
        }
        Ok(string) => match f64::from_str(string.trim()) {
            Err(err) => {
                eprintln!("read {path}: {err}");
                None
            }
            Ok(val) => {
                if !val.is_finite() {
                    eprintln!("read {path}: {val} is not finite");
                    None
                } else {
                    Some(val)
                }
            }
        },
    }
}

fn write_str(dir_path: &str, var_name: &str, val: Option<&str>) {
    let val = match val {
        Some(val) => val,
        None => return,
    };

    if let Err(err) = fs::create_dir(dir_path) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            eprintln!("mkdir {dir_path}: {err}");
            return;
        }
    }

    // Write to a temporary path first.
    let dot_path = format!("{dir_path}/.{var_name}");
    if let Err(err) = fs::write(&dot_path, format!("{val}\n")) {
        eprintln!("write {dot_path}: {err}");
        return;
    }

    // Then move into place for atomicity.
    let final_path = format!("{dir_path}/{var_name}");
    if let Err(err) = fs::rename(&dot_path, &final_path) {
        eprintln!("rename {dot_path} -> {final_path}: {err}");
    }
}

fn write_f64(dir_path: &str, var_name: &str, val: Option<f64>) {
    if let Some(val) = val {
        write_str(dir_path, var_name, Some(&val.to_string()))
    }
}

fn main() {
    // Read /etc/vpower.toml
    let config_path = "/etc/vpower.toml";
    let mut request_shutdown_battery_percent = 0.49999998;
    let mut force_shutdown_timeout_secs = 10.0;

    match fs::read(config_path) {
        Err(err) => eprintln!("read {config_path}: {err}"),

        Ok(bytes) => match toml::from_slice::<Config>(&bytes) {
            Err(err) => eprintln!("read {config_path}: {err}"),

            Ok(config) => {
                if let Some(value) = config.request_shutdown_battery_percent {
                    request_shutdown_battery_percent = value;
                }
                if let Some(value) = config.force_shutdown_timeout_secs {
                    force_shutdown_timeout_secs = value;
                }
            }
        },
    }

    println!("request_shutdown_battery_percent: {request_shutdown_battery_percent}");
    println!("force_shutdown_timeout_secs: {force_shutdown_timeout_secs}");

    // Initialize libsensors.
    let sensors = Sensors::new();

    // Keep for heuristics.
    let mut prev_ac_status: Option<&str> = None;
    let mut prev_battery_percent: Option<f64> = None;

    // Start.
    println!("Running.");

    // Every second:
    loop {
        // Read battery variables.
        let charge_full = read_battery_f64("charge_full");
        let charge_now = read_battery_f64("charge_now");
        let current_now = read_battery_f64("current_now");
        let pdam = sensors.pdam();
        let pdcs = sensors.pdcs();
        let pdvl = sensors.pdvl();
        let status = read_battery_string("status");
        let voltage_min_design = read_battery_f64("voltage_min_design");
        let voltage_now = read_battery_f64("voltage_now");

        // Derive battery variables.
        let charge_shutdown = charge_full.map(|charge_full| {
            let rsbp = request_shutdown_battery_percent;
            charge_full * (rsbp / 100.0)
        });

        let power_now = match (voltage_now, current_now) {
            (Some(voltage_now), Some(current_now)) => Some(voltage_now * current_now),
            _ => None,
        };

        // Calculate ac_status.
        let ac_status = if let Some(pdcs) = pdcs {
            let connected = (pdcs & (1 << 0)) != 0;
            let sink = (pdcs & (1 << 4)) == 0;
            if connected && sink {
                let was_disconnected = prev_ac_status == Some("Disconnected");
                let pd_power = match (pdvl, pdam) {
                    (Some(pdvl), Some(pdam)) => pdvl * pdam, // Watts.
                    _ => 0.0,
                };

                // Basically all power supplies get reported as low power for ~0.5 seconds
                // after connecting, so ignore it on the first iteration after connecting.
                if !was_disconnected && pd_power > 0.0 && pd_power < 30.0 {
                    Some("Connected slow")
                } else {
                    Some("Connected")
                }
            } else {
                Some("Disconnected")
            }
        } else {
            match status.as_deref() {
                Some("Full" | "Charging") => Some("Connected"),
                Some("Discharging") => Some("Disconnected"),
                _ => None,
            }
        };

        // Calculate battery_percent.
        let battery_percent = match (charge_now, charge_full) {
            (Some(charge_now), Some(charge_full)) => Some(charge_now / charge_full * 100.0),
            _ => None,
        };

        // Calculate battery_status.
        let battery_status = match (ac_status, status.as_deref()) {
            (_, Some("Full")) => Some("Full"),
            (_, Some("Discharging")) => Some("Discharging"),
            (Some("Connected"), Some("Charging")) => Some("Charging"),
            _ => {
                // Probably "Unknown" or "Not charging". Use heuristics as a fallback.
                let ordering = match (battery_percent, prev_battery_percent) {
                    (Some(lhs), Some(rhs)) => lhs.partial_cmp(&rhs),
                    _ => None,
                };
                match ordering {
                    Some(Ordering::Less) => Some("Discharging"),
                    Some(Ordering::Greater) => Some("Charging"),
                    _ => {
                        if battery_percent.unwrap_or(0.0) >= 89.5 {
                            // Some batteries won't charge when plugged in above ~90%.
                            // We call this "Full".
                            Some("Full")
                        } else {
                            None
                        }
                    }
                }
            }
        };

        // Calculate secs_until_battery_full.
        let vars = (charge_full, charge_now, voltage_min_design, power_now);
        let secs_until_battery_full = match vars {
            (Some(charge_full), Some(charge_now), Some(voltage_min_design), Some(power_now)) => {
                let charge_delta = charge_full - charge_now;
                let hours = charge_delta * voltage_min_design / power_now;
                Some(hours * 3600.0)
            }
            _ => None,
        };

        // Calcuate secs_until_shutdown_request.
        let vars = (charge_now, charge_shutdown, voltage_min_design, power_now);
        let secs_until_shutdown_request = match vars {
            (
                Some(charge_now),
                Some(charge_shutdown),
                Some(voltage_min_design),
                Some(power_now),
            ) => {
                if charge_now > charge_shutdown {
                    let charge_delta = charge_now - charge_shutdown;
                    let hours = charge_delta * voltage_min_design / power_now;
                    Some(hours * 3600.0)
                } else {
                    match ac_status {
                        // Avoid shutdown request while connected.
                        Some("Connected") => Some(1.0),
                        _ => Some(0.0),
                    }
                }
            }
            _ => None,
        };

        // Write to /run/vpower/*
        let dir_path = "/run/vpower";
        write_str(dir_path, "ac_status", ac_status);
        write_f64(dir_path, "battery_percent", battery_percent);
        write_str(dir_path, "battery_status", battery_status);

        let val = secs_until_battery_full;
        write_f64(dir_path, "secs_until_battery_full", val);

        let val = secs_until_shutdown_request;
        write_f64(dir_path, "secs_until_shutdown_request", val);

        // Force shutdown after timeout.
        if secs_until_shutdown_request.map_or(false, |x| x == 0.0) {
            println!("Reached {request_shutdown_battery_percent}% battery.");
            println!("Forcing shutdown in {force_shutdown_timeout_secs} seconds.");
            thread::sleep(Duration::from_secs_f64(force_shutdown_timeout_secs));

            println!("Shutting down now.");
            match Command::new("poweroff").status() {
                Err(err) => panic!("poweroff: {err}"),
                Ok(status) => match status.success() {
                    false => panic!("poweroff: {status}"),
                    true => return,
                },
            }
        }

        // Update prev_*.
        prev_ac_status = ac_status;
        prev_battery_percent = battery_percent;

        // Sleep until next iteration.
        thread::sleep(Duration::from_secs(1));
    }
}
