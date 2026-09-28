//! Reading the host's CPU package temperature and load average (#628).
//!
//! This module observes the host: it walks sysfs and procfs and never
//! decides anything about what it finds. Whether a reading admits a launch
//! is the caller's judgment (ostrom's `AdmissionLimits::decide`, over the
//! umwelt/ostrom boundary in docs/loops.md).
//!
//! Both roots are parameters, never a hardcoded `/sys` or `/proc`, so a
//! caller can inject a fixture tree in a test without touching the real
//! host.

use std::{fs, path::Path};

/// A CPU temperature and a load-per-cpu reading, each independently readable
/// or not: a machine can have working hwmon sensors and no `/proc/loadavg`
/// support, or the reverse.
#[derive(Debug, Clone, PartialEq)]
pub struct HostResourceReading {
    pub cpu_temp_c: Result<f64, String>,
    pub load_per_cpu: Result<f64, String>,
}

/// Read both host resource signals under the given sysfs/procfs roots.
#[must_use]
pub fn read_host_resources(sys_root: &Path, proc_root: &Path) -> HostResourceReading {
    HostResourceReading {
        cpu_temp_c: read_cpu_temp_c(sys_root),
        load_per_cpu: read_load_per_cpu(proc_root, sys_root),
    }
}

/// The CPU package temperature, in degrees Celsius.
///
/// Sensor priority, most specific first:
/// 1. an hwmon `tempN_label` reading exactly `Package id 0` (coretemp's own
///    name for the package sensor) — its paired `tempN_input`;
/// 2. a thermal zone whose `type` is `x86_pkg_temp`;
/// 3. otherwise the maximum over every readable hwmon `temp*_input` and
///    thermal zone `temp` file.
///
/// The named sensor is preferred over the plain maximum because an unrelated
/// zone (`acpitz`, a case or SSD sensor) commonly reads hotter than the CPU
/// package under normal load, and a plain maximum would then track a sensor
/// that says nothing about the CPU the admission limit means to protect.
/// Falling back to the maximum only when neither named sensor exists keeps a
/// machine with no coretemp or x86_pkg_temp support covered, at the cost of
/// possibly tracking a hotter unrelated sensor there — the least bad default
/// when nothing names the package explicitly.
#[must_use]
pub fn read_cpu_temp_c(sys_root: &Path) -> Result<f64, String> {
    if let Some(value) = read_labelled_hwmon_temp(sys_root, "Package id 0") {
        return Ok(value);
    }
    if let Some(value) = read_thermal_zone_temp(sys_root, "x86_pkg_temp") {
        return Ok(value);
    }
    let mut maximum: Option<f64> = None;
    for value in hwmon_temp_inputs(sys_root)
        .into_iter()
        .chain(thermal_zone_temps(sys_root))
    {
        maximum = Some(maximum.map_or(value, |current: f64| current.max(value)));
    }
    maximum.ok_or_else(|| {
        format!(
            "no readable CPU temperature sensor under {}",
            sys_root.display()
        )
    })
}

/// The 1-minute load average divided by the online CPU count.
#[must_use]
pub fn read_load_per_cpu(proc_root: &Path, sys_root: &Path) -> Result<f64, String> {
    let loadavg_path = proc_root.join("loadavg");
    let contents = fs::read_to_string(&loadavg_path)
        .map_err(|error| format!("could not read {}: {error}", loadavg_path.display()))?;
    let one_minute = contents
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .ok_or_else(|| {
            format!(
                "could not parse 1-minute load from {}",
                loadavg_path.display()
            )
        })?;
    let cpus = online_cpu_count(sys_root)?;
    if cpus == 0 {
        return Err(format!(
            "no online CPUs reported under {}",
            sys_root.display()
        ));
    }
    Ok(one_minute / f64::from(cpus))
}

/// The hwmon `tempN_input` whose sibling `tempN_label` trims to exactly `label`.
fn read_labelled_hwmon_temp(sys_root: &Path, label: &str) -> Option<f64> {
    let hwmon_root = sys_root.join("class/hwmon");
    let entries = fs::read_dir(&hwmon_root).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let device = entry.path();
        let Ok(device_entries) = fs::read_dir(&device) else {
            continue;
        };
        for device_entry in device_entries.filter_map(Result::ok) {
            let name = device_entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(index) = name
                .strip_prefix("temp")
                .and_then(|rest| rest.strip_suffix("_label"))
            else {
                continue;
            };
            let label_path = device.join(name);
            let Ok(label_contents) = fs::read_to_string(&label_path) else {
                continue;
            };
            if label_contents.trim() != label {
                continue;
            }
            let input_path = device.join(format!("temp{index}_input"));
            if let Some(value) = read_millidegrees(&input_path) {
                return Some(value);
            }
        }
    }
    None
}

/// The `temp` file of the thermal zone whose `type` trims to exactly `zone_type`.
fn read_thermal_zone_temp(sys_root: &Path, zone_type: &str) -> Option<f64> {
    let thermal_root = sys_root.join("class/thermal");
    let entries = fs::read_dir(&thermal_root).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let zone = entry.path();
        let Some(name) = zone.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let Ok(kind) = fs::read_to_string(zone.join("type")) else {
            continue;
        };
        if kind.trim() != zone_type {
            continue;
        }
        if let Some(value) = read_millidegrees(&zone.join("temp")) {
            return Some(value);
        }
    }
    None
}

/// Every readable hwmon `temp*_input`, in millidegrees converted to degrees.
fn hwmon_temp_inputs(sys_root: &Path) -> Vec<f64> {
    let hwmon_root = sys_root.join("class/hwmon");
    let Ok(entries) = fs::read_dir(&hwmon_root) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let device = entry.path();
        let Ok(device_entries) = fs::read_dir(&device) else {
            continue;
        };
        for device_entry in device_entries.filter_map(Result::ok) {
            let name = device_entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("temp") && name.ends_with("_input") {
                if let Some(value) = read_millidegrees(&device.join(name)) {
                    values.push(value);
                }
            }
        }
    }
    values
}

/// Every readable thermal zone `temp`, in millidegrees converted to degrees.
fn thermal_zone_temps(sys_root: &Path) -> Vec<f64> {
    let thermal_root = sys_root.join("class/thermal");
    let Ok(entries) = fs::read_dir(&thermal_root) else {
        return Vec::new();
    };
    let mut values = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let zone = entry.path();
        let Some(name) = zone.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("thermal_zone") {
            continue;
        }
        if let Some(value) = read_millidegrees(&zone.join("temp")) {
            values.push(value);
        }
    }
    values
}

/// Both sysfs conventions report temperature in millidegrees Celsius.
fn read_millidegrees(path: &Path) -> Option<f64> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|value| value / 1000.0)
}

/// The count of online CPUs: `devices/system/cpu/online`'s range list
/// (`"0-3,6"`), falling back to counting `cpuN` directories.
fn online_cpu_count(sys_root: &Path) -> Result<u32, String> {
    let online_path = sys_root.join("devices/system/cpu/online");
    if let Ok(contents) = fs::read_to_string(&online_path)
        && let Some(count) = parse_cpu_range_count(contents.trim())
    {
        return Ok(count);
    }
    let cpu_root = sys_root.join("devices/system/cpu");
    let entries = fs::read_dir(&cpu_root)
        .map_err(|error| format!("could not read {}: {error}", cpu_root.display()))?;
    let count = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("cpu").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
                })
            })
        })
        .count();
    u32::try_from(count).map_err(|_| format!("CPU count overflow under {}", cpu_root.display()))
}

/// Parse a sysfs CPU range list such as `"0-3,6,8-9"` into a total count.
fn parse_cpu_range_count(value: &str) -> Option<u32> {
    if value.is_empty() {
        return None;
    }
    let mut total: u32 = 0;
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<u32>().ok()?;
            let end = end.parse::<u32>().ok()?;
            if end < start {
                return None;
            }
            total = total.checked_add(end - start + 1)?;
        } else {
            part.parse::<u32>().ok()?;
            total = total.checked_add(1)?;
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{read_cpu_temp_c, read_host_resources, read_load_per_cpu};

    fn write(path: &std::path::Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, contents).expect("write fixture file");
    }

    #[test]
    fn a_coretemp_package_sensor_is_preferred_over_a_hotter_unrelated_zone() {
        let root = tempdir().expect("fixture root");
        let sys = root.path();
        // acpitz reads hotter but says nothing about the CPU package.
        write(&sys.join("class/thermal/thermal_zone0/type"), "acpitz\n");
        write(&sys.join("class/thermal/thermal_zone0/temp"), "95000\n");
        write(
            &sys.join("class/hwmon/hwmon0/temp1_label"),
            "Package id 0\n",
        );
        write(&sys.join("class/hwmon/hwmon0/temp1_input"), "62500\n");
        write(&sys.join("class/hwmon/hwmon0/temp2_label"), "Core 0\n");
        write(&sys.join("class/hwmon/hwmon0/temp2_input"), "60000\n");

        let reading = read_cpu_temp_c(sys).expect("readable coretemp package sensor");
        assert_eq!(
            reading, 62.5,
            "the labelled package sensor wins, not the hotter acpitz zone"
        );
    }

    #[test]
    fn an_x86_pkg_temp_zone_is_used_when_no_coretemp_label_exists() {
        let root = tempdir().expect("fixture root");
        let sys = root.path();
        write(&sys.join("class/thermal/thermal_zone0/type"), "acpitz\n");
        write(&sys.join("class/thermal/thermal_zone0/temp"), "95000\n");
        write(
            &sys.join("class/thermal/thermal_zone1/type"),
            "x86_pkg_temp\n",
        );
        write(&sys.join("class/thermal/thermal_zone1/temp"), "70000\n");

        let reading = read_cpu_temp_c(sys).expect("readable x86_pkg_temp zone");
        assert_eq!(reading, 70.0);
    }

    #[test]
    fn the_maximum_reading_is_used_when_neither_named_sensor_exists() {
        let root = tempdir().expect("fixture root");
        let sys = root.path();
        write(&sys.join("class/thermal/thermal_zone0/type"), "acpitz\n");
        write(&sys.join("class/thermal/thermal_zone0/temp"), "45000\n");
        write(&sys.join("class/hwmon/hwmon0/temp1_label"), "Core 0\n");
        write(&sys.join("class/hwmon/hwmon0/temp1_input"), "51000\n");

        let reading = read_cpu_temp_c(sys).expect("readable fallback sensors");
        assert_eq!(reading, 51.0);
    }

    #[test]
    fn no_sensor_files_at_all_is_unreadable() {
        let root = tempdir().expect("fixture root");
        let error = read_cpu_temp_c(root.path()).expect_err("no sensors exist in this fixture");
        assert!(
            error.contains("no readable CPU temperature sensor"),
            "{error}"
        );
    }

    #[test]
    fn load_per_cpu_divides_the_one_minute_average_by_the_online_count() {
        let root = tempdir().expect("fixture root");
        let sys = root.path().join("sys");
        let proc = root.path().join("proc");
        write(&proc.join("loadavg"), "3.2 2.1 1.0 4/512 12345\n");
        write(&sys.join("devices/system/cpu/online"), "0-3\n");

        let reading = read_load_per_cpu(&proc, &sys).expect("readable loadavg and online count");
        assert_eq!(reading, 0.8);
    }

    #[test]
    fn load_per_cpu_falls_back_to_counting_cpu_directories() {
        let root = tempdir().expect("fixture root");
        let sys = root.path().join("sys");
        let proc = root.path().join("proc");
        write(&proc.join("loadavg"), "2.0 1.0 0.5 1/200 999\n");
        for cpu in ["cpu0", "cpu1", "cpuidle"] {
            fs::create_dir_all(sys.join("devices/system/cpu").join(cpu)).expect("create cpu dir");
        }

        let reading = read_load_per_cpu(&proc, &sys).expect("readable via directory fallback");
        assert_eq!(
            reading, 1.0,
            "cpuidle is not a numbered CPU and must not be counted"
        );
    }

    #[test]
    fn an_unreadable_loadavg_is_reported_not_defaulted() {
        let root = tempdir().expect("fixture root");
        let sys = root.path().join("sys");
        let proc = root.path().join("proc");
        fs::create_dir_all(&proc).expect("create empty proc root");

        let error = read_load_per_cpu(&proc, &sys).expect_err("no loadavg file exists");
        assert!(error.contains("loadavg"), "{error}");
    }

    #[test]
    fn read_host_resources_reads_both_signals_independently() {
        let root = tempdir().expect("fixture root");
        let sys = root.path().join("sys");
        let proc = root.path().join("proc");
        write(
            &sys.join("class/thermal/thermal_zone0/type"),
            "x86_pkg_temp\n",
        );
        write(&sys.join("class/thermal/thermal_zone0/temp"), "55000\n");
        write(&sys.join("devices/system/cpu/online"), "0-1\n");
        write(&proc.join("loadavg"), "1.0 0.5 0.2 1/50 1\n");

        let reading = read_host_resources(&sys, &proc);
        assert_eq!(reading.cpu_temp_c, Ok(55.0));
        assert_eq!(reading.load_per_cpu, Ok(0.5));
    }
}
