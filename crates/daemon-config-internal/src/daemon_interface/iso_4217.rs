//! The active alphabetic codes of ISO 4217, the codes of the currencies and
//! the funds that the ISO 4217 maintenance agency lists in its List One, as
//! of October 2026. A code that the agency withdraws, such as `HRK` after
//! Croatia took the euro, is not in it.

/// The active alphabetic codes of ISO 4217, in alphabetical order.
pub(super) const ACTIVE_CODES: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT", "BHD",
    "BIF", "BMD", "BND", "BOB", "BOV", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD", "CDF",
    "CHE", "CHF", "CHW", "CLF", "CLP", "CNY", "COP", "COU", "CRC", "CUP", "CVE", "CZK", "DJF",
    "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL", "GHS", "GIP",
    "GMD", "GNF", "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF", "IDR", "ILS", "INR", "IQD", "IRR",
    "ISK", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW", "KWD", "KYD", "KZT",
    "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MMK", "MNT", "MOP",
    "MRU", "MUR", "MVR", "MWK", "MXN", "MXV", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK", "NPR",
    "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR", "RON", "RSD", "RUB",
    "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SOS", "SRD", "SSP", "STN",
    "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD", "TWD", "TZS", "UAH",
    "UGX", "USD", "USN", "UYI", "UYU", "UYW", "UZS", "VED", "VES", "VND", "VUV", "WST", "XAF",
    "XAG", "XAU", "XBA", "XBB", "XBC", "XBD", "XCD", "XCG", "XDR", "XOF", "XPD", "XPF", "XPT",
    "XSU", "XTS", "XUA", "XXX", "YER", "ZAR", "ZMW", "ZWG",
];

/// The active code `code`, written as ISO 4217 writes it: three capital
/// letters.
pub(super) fn active_code(code: &str) -> Option<&'static str> {
    ACTIVE_CODES
        .binary_search(&code)
        .ok()
        .map(|index| ACTIVE_CODES[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is sorted with each code once, so the binary search of
    /// [`active_code`] finds each one, and each code is three capital
    /// letters.
    #[test]
    fn the_active_codes_are_sorted_unique_and_three_capital_letters() {
        assert!(
            ACTIVE_CODES.windows(2).all(|pair| pair[0] < pair[1]),
            "sorted, each code once"
        );
        for code in ACTIVE_CODES {
            assert!(
                code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_uppercase()),
                "{code}"
            );
            assert_eq!(active_code(code), Some(*code));
        }
        assert_eq!(ACTIVE_CODES.len(), 177);
    }

    #[test]
    fn the_codes_of_the_currencies_in_use_are_active() {
        for code in [
            "USD", "EUR", "JPY", "CNY", "GBP", "CHF", "CAD", "AUD", "KRW", "INR", "BRL", "XCG",
            "ZWG", "SLE", "VED",
        ] {
            assert_eq!(active_code(code), Some(code), "{code}");
        }
    }

    /// A withdrawn code, a lowercase code and a code that ISO 4217 never
    /// gave are not active.
    #[test]
    fn withdrawn_unknown_and_lowercase_codes_are_not_active() {
        for code in [
            "HRK", "BGN", "ANG", "ZWL", "SLL", "CUC", "VEF", "MRO", "STD", "LTL", "LVL", "DEM",
            "FRF", "usd", "Usd", "USX", "US", "USDD", "",
        ] {
            assert_eq!(active_code(code), None, "{code}");
        }
    }
}
