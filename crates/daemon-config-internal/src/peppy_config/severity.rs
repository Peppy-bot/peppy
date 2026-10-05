//! The severity scale of exported log records.

use super::setting::null_or_string;
use serde::{Deserializer, Serialize, Serializer};

/// The severity of an exported log record, on the OpenTelemetry scale, from
/// the least severe to the most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    /// Every severity, from the least severe to the most.
    pub const ALL: [Severity; 6] = [
        Severity::Trace,
        Severity::Debug,
        Severity::Info,
        Severity::Warn,
        Severity::Error,
        Severity::Fatal,
    ];

    /// The OpenTelemetry `SeverityNumber` of the severity.
    pub fn number(self) -> i32 {
        match self {
            Severity::Trace => 1,
            Severity::Debug => 5,
            Severity::Info => 9,
            Severity::Warn => 13,
            Severity::Error => 17,
            Severity::Fatal => 21,
        }
    }

    /// The lowercase name of the severity.
    pub fn name(self) -> &'static str {
        match self {
            Severity::Trace => "trace",
            Severity::Debug => "debug",
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
            Severity::Fatal => "fatal",
        }
    }
}

/// The `otlp_min_severity` setting: the lowest severity the daemon exports.
/// It goes up to warn, so the export's own report of discarded records, a
/// warning, is exported under every setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpMinSeverity {
    Trace,
    Debug,
    Info,
    Warn,
}

impl OtlpMinSeverity {
    /// Every value of the setting, from the least severe to the most.
    pub const ALL: [OtlpMinSeverity; 4] = [
        OtlpMinSeverity::Trace,
        OtlpMinSeverity::Debug,
        OtlpMinSeverity::Info,
        OtlpMinSeverity::Warn,
    ];

    /// The severity the value names.
    pub fn severity(self) -> Severity {
        match self {
            OtlpMinSeverity::Trace => Severity::Trace,
            OtlpMinSeverity::Debug => Severity::Debug,
            OtlpMinSeverity::Info => Severity::Info,
            OtlpMinSeverity::Warn => Severity::Warn,
        }
    }

    /// The lowercase name of the value.
    pub fn name(self) -> &'static str {
        self.severity().name()
    }
}

impl Serialize for OtlpMinSeverity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

/// Deserializes the `otlp_min_severity` setting: `null` or the lowercase
/// name of one of its values.
pub(super) fn deserialize_otlp_min_severity<'de, D>(
    deserializer: D,
) -> Result<Option<OtlpMinSeverity>, D::Error>
where
    D: Deserializer<'de>,
{
    let names: Vec<String> = OtlpMinSeverity::ALL
        .iter()
        .map(|minimum| format!("{:?}", minimum.name()))
        .collect();
    null_or_string(
        deserializer,
        "otlp_min_severity",
        &format!("one of {}, or null", names.join(", ")),
        |value| {
            if let Some(minimum) = OtlpMinSeverity::ALL
                .into_iter()
                .find(|minimum| minimum.name() == value)
            {
                return Ok(minimum);
            }
            if Severity::ALL
                .iter()
                .any(|severity| severity.name() == value)
            {
                return Err(format!("is above {:?}", OtlpMinSeverity::Warn.name()));
            }
            Err("is not one of the lowercase names".to_owned())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{OtlpMinSeverity, Severity};

    #[test]
    fn severities_order_by_their_number() {
        let numbers: Vec<i32> = Severity::ALL.into_iter().map(Severity::number).collect();
        assert_eq!(numbers, [1, 5, 9, 13, 17, 21]);
        assert!(Severity::ALL.is_sorted());
    }

    #[test]
    fn the_setting_goes_up_to_warn() {
        let severities: Vec<Severity> = OtlpMinSeverity::ALL
            .into_iter()
            .map(OtlpMinSeverity::severity)
            .collect();
        assert_eq!(severities, Severity::ALL[..4]);
    }
}
