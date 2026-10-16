//! Machine stats (docs/protocol.md "Machine stats"): CPU, memory and disk of
//! the machine the runner is on, sent only while someone is looking. Numbers
//! only: no process names, command lines, user names or paths. A field the
//! machine cannot report is left out, never guessed. Shared fixtures:
//! `packages/protocol/fixtures/stats.json`.

use serde::{Deserialize, Serialize};

/// The hello feature of a runner that answers `stats`.
pub const FEATURE_STATS: &str = "stats";
/// Sample rate bounds (samples per second). `0` from the gateway means stop.
pub const STATS_HZ_MIN: f64 = 0.1;
pub const STATS_HZ_MAX: f64 = 0.5;

/// The rate a gateway `stats` frame asks for: `None` = stop (0, negative or
/// not a number), otherwise clamped to the bounds.
pub fn stats_rate(hz: f64) -> Option<f64> {
    if hz.is_finite() && hz > 0.0 {
        Some(hz.clamp(STATS_HZ_MIN, STATS_HZ_MAX))
    } else {
        None
    }
}

/// CPU: `pct` is the share of what the machine may use (all cores, or the
/// container's quota) since the previous sample, so the first sample has none.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StatsCpu {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f64>,
    /// Cores of the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cores: Option<u32>,
    /// The container's CPU quota in cores, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<f64>,
    /// Share of scheduler periods the container was throttled in since the previous sample.
    #[serde(rename = "throttledPct", default, skip_serializing_if = "Option::is_none")]
    pub throttled_pct: Option<f64>,
    /// One-minute load average (hosts only; inside a container it is the node's, so it is left out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1: Option<f64>,
}

/// Memory in bytes. `limited`: `limit` is a container limit (the same one the
/// memory watchdog enforces), not the size of the host's memory.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StatsMem {
    pub used: u64,
    pub limit: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limited: bool,
    /// PSI `some avg10`: percent of the last 10 s some process waited for memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure10: Option<f64>,
    #[serde(rename = "swapUsed", default, skip_serializing_if = "Option::is_none")]
    pub swap_used: Option<u64>,
}

/// One disk: `id` is a fixed word (`home`), never a path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsDisk {
    pub id: String,
    pub free: u64,
    pub total: u64,
}

/// One sample. `at` is Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StatsSample {
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<StatsCpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<StatsMem>,
    #[serde(default)]
    pub disks: Vec<StatsDisk>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_stops_on_zero_and_junk_and_clamps_the_rest() {
        assert_eq!(stats_rate(0.0), None);
        assert_eq!(stats_rate(-1.0), None);
        assert_eq!(stats_rate(f64::NAN), None);
        assert_eq!(stats_rate(f64::INFINITY), None);
        assert_eq!(stats_rate(0.5), Some(0.5));
        assert_eq!(stats_rate(50.0), Some(STATS_HZ_MAX));
        assert_eq!(stats_rate(0.001), Some(STATS_HZ_MIN));
    }

    #[test]
    fn an_empty_sample_is_small_and_names_nothing() {
        let s = StatsSample {
            at: 1,
            ..Default::default()
        };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"at":1,"disks":[]}"#);
    }
}
