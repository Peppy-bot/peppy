//! CPU / scheduling environment detection.
//!
//! On a shared host with turbo enabled, per-core frequency and noisy-neighbor
//! load, not the code, dominate absolute latency numbers. Surfacing the
//! environment makes a slow run self-explanatory. Linux-only; unknown values
//! read back as `"?"`.

use std::fmt;

const INTEL_NO_TURBO_PATH: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";
const CPUFREQ_BOOST_PATH: &str = "/sys/devices/system/cpu/cpufreq/boost";

/// A snapshot of the CPU/scheduling environment governing run-to-run variance.
#[derive(Clone, Debug)]
pub struct CpuEnvironment {
    pub cores: usize,
    pub governor: String,
    pub turbo: Turbo,
    pub loadavg: String,
}

/// Whether the CPU may clock above its base frequency, as the kernel reports
/// it. `Unknown` when neither the Intel nor the generic cpufreq switch gives a
/// state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turbo {
    On,
    Off,
    Unknown,
}

impl Turbo {
    /// Reads the Intel `intel_pstate` switch first, and the generic cpufreq
    /// switch only when the Intel one gives no state.
    fn detect() -> Self {
        if let Some(turbo) = read_trimmed(INTEL_NO_TURBO_PATH)
            .as_deref()
            .and_then(Self::from_intel_no_turbo)
        {
            return turbo;
        }
        read_trimmed(CPUFREQ_BOOST_PATH)
            .as_deref()
            .and_then(Self::from_cpufreq_boost)
            .unwrap_or(Self::Unknown)
    }

    /// Parses `intel_pstate/no_turbo`, which is `1` when turbo is disabled.
    fn from_intel_no_turbo(value: &str) -> Option<Self> {
        match value {
            "0" => Some(Self::On),
            "1" => Some(Self::Off),
            _ => None,
        }
    }

    /// Parses `cpufreq/boost`, which is `1` when turbo is enabled.
    fn from_cpufreq_boost(value: &str) -> Option<Self> {
        match value {
            "1" => Some(Self::On),
            "0" => Some(Self::Off),
            _ => None,
        }
    }
}

impl fmt::Display for Turbo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Unknown => "?",
        };
        f.write_str(text)
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

impl CpuEnvironment {
    /// Read the current environment from `available_parallelism` and sysfs.
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        let governor = read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            .unwrap_or_else(|| "?".into());
        let turbo = Turbo::detect();
        let loadavg = read_trimmed("/proc/loadavg")
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "?".into());
        Self {
            cores,
            governor,
            turbo,
            loadavg,
        }
    }

    /// One-line summary, e.g. `4 cores, governor=performance, turbo=off, loadavg=0.30`.
    pub fn summary_line(&self) -> String {
        format!(
            "{} cores, governor={}, turbo={}, loadavg={}",
            self.cores, self.governor, self.turbo, self.loadavg
        )
    }

    /// Whether the environment is likely to add noise to absolute latency:
    /// turbo not explicitly off, or a non-`performance` governor.
    pub fn is_noisy(&self) -> bool {
        self.turbo != Turbo::Off || self.governor != "performance"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(cores: usize, governor: &str, turbo: Turbo, loadavg: &str) -> CpuEnvironment {
        CpuEnvironment {
            cores,
            governor: governor.to_string(),
            turbo,
            loadavg: loadavg.to_string(),
        }
    }

    #[test]
    fn turbo_switches_parse_to_a_state() {
        let cases = [
            ("0", Some(Turbo::On), Some(Turbo::Off)),
            ("1", Some(Turbo::Off), Some(Turbo::On)),
            ("2", None, None),
            ("", None, None),
        ];
        for (value, from_intel_no_turbo, from_cpufreq_boost) in cases {
            assert_eq!(
                Turbo::from_intel_no_turbo(value),
                from_intel_no_turbo,
                "intel_pstate/no_turbo = {value:?}"
            );
            assert_eq!(
                Turbo::from_cpufreq_boost(value),
                from_cpufreq_boost,
                "cpufreq/boost = {value:?}"
            );
        }
    }

    #[test]
    fn detect_does_not_panic_on_this_host() {
        // Smoke test: detect() reads live sysfs, so its values are host-specific;
        // we only assert it produces the expected shape without panicking. The
        // behavior of summary_line()/is_noisy() is pinned by the fixture tests
        // below, independent of the host.
        let line = CpuEnvironment::detect().summary_line();
        assert!(line.contains("cores"));
        assert!(line.contains("governor="));
        assert!(line.contains("turbo="));
    }

    #[test]
    fn turbo_states_print_as_their_summary_words() {
        for (turbo, word) in [
            (Turbo::On, "on"),
            (Turbo::Off, "off"),
            (Turbo::Unknown, "?"),
        ] {
            assert_eq!(turbo.to_string(), word);
        }
    }

    #[test]
    fn summary_line_formats_all_fields() {
        assert_eq!(
            env(4, "performance", Turbo::Off, "0.30").summary_line(),
            "4 cores, governor=performance, turbo=off, loadavg=0.30"
        );
    }

    #[test]
    fn is_noisy_unless_turbo_off_and_governor_performance() {
        // Quiet only when turbo is explicitly off AND the governor is performance.
        assert!(!env(4, "performance", Turbo::Off, "0.10").is_noisy());
        // Turbo on (or unknown) is noisy regardless of governor.
        assert!(env(4, "performance", Turbo::On, "0.10").is_noisy());
        assert!(env(4, "performance", Turbo::Unknown, "0.10").is_noisy());
        // A non-performance (or unknown) governor is noisy even with turbo off.
        assert!(env(4, "powersave", Turbo::Off, "0.10").is_noisy());
        assert!(env(4, "?", Turbo::Off, "0.10").is_noisy());
    }
}
