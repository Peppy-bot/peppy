//! CPU / scheduling environment detection.
//!
//! On a shared host with turbo enabled, per-core frequency and noisy-neighbor
//! load — not the code — dominate absolute latency numbers. Surfacing the
//! environment makes a slow run self-explanatory. Linux-only; unknown values
//! read back as `"?"`.

/// A snapshot of the CPU/scheduling environment governing run-to-run variance.
#[derive(Clone, Debug)]
pub struct CpuEnvironment {
    pub cores: usize,
    pub governor: String,
    pub turbo: String,
    pub loadavg: String,
}

fn read1(path: &str) -> Option<String> {
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
        let governor = read1("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            .unwrap_or_else(|| "?".into());
        let turbo = match read1("/sys/devices/system/cpu/intel_pstate/no_turbo").as_deref() {
            Some("0") => "on",
            Some("1") => "off",
            _ => match read1("/sys/devices/system/cpu/cpufreq/boost").as_deref() {
                Some("1") => "on",
                Some("0") => "off",
                _ => "?",
            },
        }
        .to_string();
        let loadavg = read1("/proc/loadavg")
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
        self.turbo != "off" || self.governor != "performance"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_and_summarize() {
        let env = CpuEnvironment::detect();
        let line = env.summary_line();
        assert!(line.contains("cores"));
        assert!(line.contains("governor="));
        assert!(line.contains("turbo="));
        // is_noisy must not panic and is deterministic for a given snapshot.
        let _ = env.is_noisy();
    }
}
