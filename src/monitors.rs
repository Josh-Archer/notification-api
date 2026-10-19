//! Named heartbeat monitor configuration.
//!
//! The `poop` monitor always exists and takes its defaults from the legacy
//! `HEARTBEAT_TIMEOUT_SECS`, `DEBOUNCE_SECS`, `OUTAGE_MESSAGE` and
//! `RECOVERY_MESSAGE` variables, so existing deployments do not change. Extra
//! monitors come from `MONITORS_JSON` and `MONITOR_<NAME>_*` variables.
//! Precedence per field: legacy/default < `MONITORS_JSON` < `MONITOR_<NAME>_*`.

use serde::Deserialize;
use std::collections::BTreeMap;

/// Name of the monitor behind the original `/heartbeat/poop` route.
pub const DEFAULT_MONITOR: &str = "poop";

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 90;
pub(crate) const DEFAULT_DEBOUNCE_SECS: u64 = 300;
pub(crate) const DEFAULT_OUTAGE_MESSAGE: &str = "❌ Poop Monitor is offline!";
pub(crate) const DEFAULT_RECOVERY_MESSAGE: &str = "✅ Poop Monitor is back online!";

/// Settings for one named monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorConfig {
    pub timeout_secs: u64,
    pub debounce_secs: u64,
    pub outage_message: String,
    pub recovery_message: String,
    /// Per-monitor bearer secret; `None` falls back to `HEARTBEAT_SECRET`.
    pub secret: Option<String>,
}

impl MonitorConfig {
    /// Defaults for a monitor that is not `poop`.
    fn for_name(name: &str, timeout_secs: u64, debounce_secs: u64) -> Self {
        Self {
            timeout_secs,
            debounce_secs,
            outage_message: format!("❌ Monitor '{name}' is offline!"),
            recovery_message: format!("✅ Monitor '{name}' is back online!"),
            secret: None,
        }
    }
}

/// Canonical monitor name: lowercase, `-` folded to `_`. Returns `None` when
/// the name is empty or contains anything but `[A-Za-z0-9_-]`.
pub fn normalize_name(name: &str) -> Option<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name.to_ascii_lowercase().replace('-', "_"))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct MonitorOverride {
    timeout_secs: Option<u64>,
    debounce_secs: Option<u64>,
    outage_message: Option<String>,
    recovery_message: Option<String>,
    secret: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn parse_secs(key: &str, value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{key} must be a number"))
}

/// Build the monitor table from environment-style variables.
///
/// `vars` is a snapshot of the environment (name → value). Errors are
/// human-readable startup failures.
pub fn load_monitors(
    vars: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, MonitorConfig>, String> {
    let get = |key: &str| vars.get(key).filter(|v| !v.is_empty()).cloned();
    let secs = |key: &str, default: u64| -> Result<u64, String> {
        match vars.get(key) {
            Some(v) => parse_secs(key, v),
            None => Ok(default),
        }
    };

    let timeout = secs("HEARTBEAT_TIMEOUT_SECS", DEFAULT_TIMEOUT_SECS)?;
    let debounce = secs("DEBOUNCE_SECS", DEFAULT_DEBOUNCE_SECS)?;

    let mut monitors = BTreeMap::new();
    monitors.insert(
        DEFAULT_MONITOR.to_string(),
        MonitorConfig {
            timeout_secs: timeout,
            debounce_secs: debounce,
            outage_message: get("OUTAGE_MESSAGE").unwrap_or_else(|| DEFAULT_OUTAGE_MESSAGE.into()),
            recovery_message: get("RECOVERY_MESSAGE")
                .unwrap_or_else(|| DEFAULT_RECOVERY_MESSAGE.into()),
            secret: None,
        },
    );

    if let Some(json) = get("MONITORS_JSON") {
        let parsed: BTreeMap<String, MonitorOverride> = serde_json::from_str(&json)
            .map_err(|err| format!("MONITORS_JSON must be an object of monitor settings: {err}"))?;
        for (raw_name, over) in parsed {
            let name = normalize_name(&raw_name)
                .ok_or_else(|| format!("invalid monitor name '{raw_name}' in MONITORS_JSON"))?;
            let entry = monitors
                .entry(name.clone())
                .or_insert_with(|| MonitorConfig::for_name(&name, timeout, debounce));
            apply(entry, over);
        }
    }

    // MONITOR_<NAME>_<FIELD> variables; the field suffix is matched first so
    // names may themselves contain underscores.
    const SUFFIXES: [&str; 5] = [
        "_TIMEOUT_SECS",
        "_DEBOUNCE_SECS",
        "_OUTAGE_MESSAGE",
        "_RECOVERY_MESSAGE",
        "_SECRET",
    ];
    for (key, value) in vars {
        let Some(rest) = key.strip_prefix("MONITOR_") else {
            continue;
        };
        let Some(suffix) = SUFFIXES.iter().find(|s| rest.ends_with(**s)) else {
            continue;
        };
        let raw_name = &rest[..rest.len() - suffix.len()];
        let Some(name) = normalize_name(raw_name) else {
            return Err(format!("invalid monitor name in {key}"));
        };
        if value.is_empty() {
            continue;
        }
        let entry = monitors
            .entry(name.clone())
            .or_insert_with(|| MonitorConfig::for_name(&name, timeout, debounce));
        let mut over = MonitorOverride::default();
        match *suffix {
            "_TIMEOUT_SECS" => over.timeout_secs = Some(parse_secs(key, value)?),
            "_DEBOUNCE_SECS" => over.debounce_secs = Some(parse_secs(key, value)?),
            "_OUTAGE_MESSAGE" => over.outage_message = Some(value.clone()),
            "_RECOVERY_MESSAGE" => over.recovery_message = Some(value.clone()),
            _ => over.secret = Some(value.clone()),
        }
        apply(entry, over);
    }

    Ok(monitors)
}

fn apply(entry: &mut MonitorConfig, over: MonitorOverride) {
    if let Some(v) = over.timeout_secs {
        entry.timeout_secs = v;
    }
    if let Some(v) = over.debounce_secs {
        entry.debounce_secs = v;
    }
    if let Some(v) = non_empty(over.outage_message) {
        entry.outage_message = v;
    }
    if let Some(v) = non_empty(over.recovery_message) {
        entry.recovery_message = v;
    }
    if let Some(v) = non_empty(over.secret) {
        entry.secret = Some(v.trim().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn poop_only_by_default_with_legacy_values() {
        let m = load_monitors(&vars(&[])).unwrap();
        assert_eq!(m.len(), 1);
        let poop = &m["poop"];
        assert_eq!(poop.timeout_secs, 90);
        assert_eq!(poop.debounce_secs, 300);
        assert_eq!(poop.outage_message, DEFAULT_OUTAGE_MESSAGE);
        assert_eq!(poop.recovery_message, DEFAULT_RECOVERY_MESSAGE);
        assert_eq!(poop.secret, None);

        let m = load_monitors(&vars(&[
            ("HEARTBEAT_TIMEOUT_SECS", "15"),
            ("DEBOUNCE_SECS", "40"),
            ("OUTAGE_MESSAGE", "down"),
            ("RECOVERY_MESSAGE", "up"),
        ]))
        .unwrap();
        assert_eq!(m["poop"].timeout_secs, 15);
        assert_eq!(m["poop"].debounce_secs, 40);
        assert_eq!(m["poop"].outage_message, "down");
        assert_eq!(m["poop"].recovery_message, "up");
    }

    #[test]
    fn env_monitors_inherit_legacy_timing_and_get_named_messages() {
        let m = load_monitors(&vars(&[
            ("HEARTBEAT_TIMEOUT_SECS", "15"),
            ("MONITOR_BACKUP_SECRET", "s3"),
            ("MONITOR_CLEANARR_JOB_TIMEOUT_SECS", "3600"),
            ("MONITOR_CLEANARR_JOB_OUTAGE_MESSAGE", "cleanarr down"),
        ]))
        .unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m["backup"].timeout_secs, 15);
        assert_eq!(m["backup"].secret.as_deref(), Some("s3"));
        assert_eq!(
            m["backup"].outage_message,
            "❌ Monitor 'backup' is offline!"
        );
        assert_eq!(m["cleanarr_job"].timeout_secs, 3600);
        assert_eq!(m["cleanarr_job"].outage_message, "cleanarr down");
        assert_eq!(m["poop"].secret, None);
    }

    #[test]
    fn json_then_env_precedence_and_poop_override() {
        let m = load_monitors(&vars(&[
            (
                "MONITORS_JSON",
                r#"{"Backup":{"timeout_secs":100,"secret":"j"},"poop":{"timeout_secs":7}}"#,
            ),
            ("MONITOR_BACKUP_TIMEOUT_SECS", "200"),
        ]))
        .unwrap();
        assert_eq!(m["backup"].timeout_secs, 200);
        assert_eq!(m["backup"].secret.as_deref(), Some("j"));
        assert_eq!(m["poop"].timeout_secs, 7);
    }

    #[test]
    fn invalid_config_is_rejected() {
        assert!(load_monitors(&vars(&[("MONITORS_JSON", "[1]")])).is_err());
        assert!(load_monitors(&vars(&[("MONITORS_JSON", r#"{"a/b":{}}"#)])).is_err());
        assert!(load_monitors(&vars(&[("MONITORS_JSON", r#"{"a":{"bogus":1}}"#)])).is_err());
        let err = load_monitors(&vars(&[("MONITOR_X_TIMEOUT_SECS", "nope")])).unwrap_err();
        assert!(err.contains("MONITOR_X_TIMEOUT_SECS must be a number"));
        let err = load_monitors(&vars(&[("HEARTBEAT_TIMEOUT_SECS", "nope")])).unwrap_err();
        assert!(err.contains("HEARTBEAT_TIMEOUT_SECS must be a number"));
    }

    #[test]
    fn name_normalization() {
        assert_eq!(
            normalize_name("Cleanarr-Job").as_deref(),
            Some("cleanarr_job")
        );
        assert_eq!(normalize_name(""), None);
        assert_eq!(normalize_name("a b"), None);
    }
}
