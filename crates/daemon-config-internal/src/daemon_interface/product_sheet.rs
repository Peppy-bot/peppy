//! The product sheet of a robot model: what a buyer of a robot of the model
//! reads, as the `product` of an option of a `robot_membership:v1` scope
//! gives it. Every field is optional. A launcher usually writes the sheet as
//! an include, `product: { $include: "../so101/products/so101.json5" }`, so
//! that one file per model serves every launcher that offers it; the scope
//! parser reads the value of that file.

use super::iso_4217;
use super::robot_membership::MAX_DESCRIPTION_CHARS;
use chrono::NaiveDate;
use serde::Deserialize;

/// The product sheet of a robot model. Parsing holds every rule of the sheet
/// but the one of its dates against today, which
/// [`ProductSheet::dates_after`] holds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawProductSheet")]
pub struct ProductSheet {
    vendor: Option<Vendor>,
    purchase_url: Option<PurchaseUrl>,
    price: Option<Price>,
    availability: Option<Availability>,
    mass_kg: Option<PositiveNumber>,
    height_m: Option<PositiveNumber>,
}

/// The price of the offer that the `purchase_url` of a sheet opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    amount: PositiveNumber,
    currency: Currency,
    checked_on: CalendarDate,
}

/// Whether the offer that the `purchase_url` of a sheet opens can be bought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Availability {
    state: AvailabilityState,
    checked_on: CalendarDate,
}

/// The state of an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    InStock,
    Preorder,
    OutOfStock,
    Discontinued,
}

impl AvailabilityState {
    /// The state as a sheet writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InStock => "in_stock",
            Self::Preorder => "preorder",
            Self::OutOfStock => "out_of_stock",
            Self::Discontinued => "discontinued",
        }
    }
}

/// Wire shape of [`ProductSheet`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields, expecting = "a product sheet object")]
struct RawProductSheet {
    vendor: Option<Vendor>,
    purchase_url: Option<PurchaseUrl>,
    price: Option<Price>,
    availability: Option<Availability>,
    mass_kg: Option<PositiveNumber>,
    height_m: Option<PositiveNumber>,
}

impl TryFrom<RawProductSheet> for ProductSheet {
    type Error = String;

    fn try_from(raw: RawProductSheet) -> Result<Self, String> {
        if raw.purchase_url.is_none() {
            for (field, given) in [
                ("price", raw.price.is_some()),
                ("availability", raw.availability.is_some()),
            ] {
                if given {
                    return Err(format!(
                        "`{field}` is a fact of the offer that `purchase_url` opens, so a sheet \
                         that gives `{field}` gives `purchase_url`"
                    ));
                }
            }
        }
        Ok(Self {
            vendor: raw.vendor,
            purchase_url: raw.purchase_url,
            price: raw.price,
            availability: raw.availability,
            mass_kg: raw.mass_kg,
            height_m: raw.height_m,
        })
    }
}

impl ProductSheet {
    /// The sheet of a model whose option gives no `product`: every field
    /// absent.
    pub(super) fn empty() -> Self {
        Self {
            vendor: None,
            purchase_url: None,
            price: None,
            availability: None,
            mass_kg: None,
            height_m: None,
        }
    }

    /// The name of the seller.
    pub fn vendor(&self) -> Option<&str> {
        self.vendor.as_ref().map(|vendor| vendor.0.as_str())
    }

    /// The `https` page of the offer, in the normal form of a URL.
    pub fn purchase_url(&self) -> Option<&str> {
        self.purchase_url.as_ref().map(|url| url.0.as_str())
    }

    /// The price of the offer.
    pub fn price(&self) -> Option<&Price> {
        self.price.as_ref()
    }

    /// Whether the offer can be bought.
    pub fn availability(&self) -> Option<&Availability> {
        self.availability.as_ref()
    }

    /// The mass in kilograms of the robot of the offer, as the vendor
    /// states it: more than 0.
    pub fn mass_kg(&self) -> Option<f64> {
        self.mass_kg.map(|mass| mass.0)
    }

    /// The height in metres of the robot of the offer, as the vendor states
    /// it: more than 0.
    pub fn height_m(&self) -> Option<f64> {
        self.height_m.map(|height| height.0)
    }

    /// One refusal for each `checked_on` of the sheet that is after
    /// `today`, named by its field. A date of the sheet says when a fact of
    /// the offer was last checked, so it cannot be in the future.
    pub fn dates_after(&self, today: NaiveDate) -> Vec<String> {
        [
            ("price", self.price.map(|price| price.checked_on)),
            (
                "availability",
                self.availability
                    .map(|availability| availability.checked_on),
            ),
        ]
        .into_iter()
        .filter_map(|(field, date)| {
            let date = date?.0;
            (date > today).then(|| {
                format!(
                    "{field}.checked_on: `{date}` is in the future: the latest date on Earth is \
                     `{today}`, and a `checked_on` is the date of a check that took place"
                )
            })
        })
        .collect()
    }
}

impl Price {
    /// The amount: more than 0.
    pub fn amount(&self) -> f64 {
        self.amount.0
    }

    /// The ISO 4217 code of the currency of the amount.
    pub fn currency(&self) -> &'static str {
        self.currency.0
    }

    /// The date on which the price was last checked.
    pub fn checked_on(&self) -> NaiveDate {
        self.checked_on.0
    }
}

impl Availability {
    /// The state of the offer.
    pub fn state(&self) -> AvailabilityState {
        self.state
    }

    /// The date on which the state was last checked.
    pub fn checked_on(&self) -> NaiveDate {
        self.checked_on.0
    }
}

/// The name of a seller: one line, not blank, at most
/// [`MAX_DESCRIPTION_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct Vendor(String);

impl TryFrom<String> for Vendor {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Err(
                "the vendor is empty; leave `vendor` out of a sheet that names no \
                        seller"
                    .to_owned(),
            );
        }
        if text.chars().any(char::is_control) {
            return Err(
                "the vendor is one line, with no line break or other control character".to_owned(),
            );
        }
        let length = text.chars().count();
        if length > MAX_DESCRIPTION_CHARS {
            return Err(format!(
                "the vendor is at most {MAX_DESCRIPTION_CHARS} characters, and this one has \
                 {length}"
            ));
        }
        Ok(Self(text))
    }
}

/// The `https` page of an offer, with no credentials, in the normal form
/// that the `url` crate gives it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct PurchaseUrl(String);

impl TryFrom<String> for PurchaseUrl {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let url = crate::internal::peppy_config::parse_https_url(&text)
            .map_err(|reason| format!("`{text}` {reason}"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(format!(
                "`{text}` holds credentials, and the page of an offer is public"
            ));
        }
        Ok(Self(url.into()))
    }
}

/// An active ISO 4217 alphabetic code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Currency(&'static str);

// Written by hand: the derive reads a `&'static str` field as one borrowed
// from the input, which the code of a currency is not.
impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::try_from(text).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for Currency {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        iso_4217::active_code(&text).map(Self).ok_or_else(|| {
            format!(
                "`{text}` is no ISO 4217 currency; write the active code of the currency in \
                 three capital letters, such as `USD` or `EUR`"
            )
        })
    }
}

/// A finite number more than 0.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "f64")]
struct PositiveNumber(f64);

// A `PositiveNumber` is finite, never NaN, so its equality is total.
impl Eq for PositiveNumber {}

impl TryFrom<f64> for PositiveNumber {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, String> {
        if !value.is_finite() {
            return Err(format!("`{value}` is not a finite number"));
        }
        if value <= 0.0 {
            return Err(format!(
                "`{value}` is 0 or less, and the value is more than 0"
            ));
        }
        Ok(Self(value))
    }
}

/// A calendar date written `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct CalendarDate(NaiveDate);

impl TryFrom<String> for CalendarDate {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let refusal = || format!("`{text}` is no calendar date written YYYY-MM-DD");
        // chrono reads a month or a day of one digit too, so the form is
        // read here first.
        let written_as_date = text.len() == 10
            && text.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                _ => byte.is_ascii_digit(),
            });
        if !written_as_date {
            return Err(refusal());
        }
        NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .map(Self)
            .map_err(|_| refusal())
    }
}

/// The time zone whose date is the latest on Earth: UTC+14, the Line Islands.
const LATEST_ZONE_HOURS: i64 = 14;

/// The latest date on Earth now, the date in UTC+14, after which
/// `peppy stack resolve` refuses a date of a product sheet and the launch
/// warns about it: a sheet dated with the date of its writer in any time
/// zone holds.
pub fn latest_date_on_earth() -> NaiveDate {
    (chrono::Utc::now() + chrono::Duration::hours(LATEST_ZONE_HOURS)).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The latest date on Earth is today's date in UTC, or the day after it
    /// from 10:00 UTC on, whatever the time zone of the machine.
    #[test]
    fn the_latest_date_on_earth_is_today_in_utc_or_the_day_after() {
        let before = chrono::Utc::now().date_naive();
        let latest = latest_date_on_earth();
        let after = chrono::Utc::now().date_naive();
        assert!(latest >= before, "{latest} {before}");
        assert!(
            latest <= after + chrono::Duration::days(1),
            "{latest} {after}"
        );
    }

    fn sheet(value: serde_json::Value) -> Result<ProductSheet, String> {
        super::super::parse_value::<ProductSheet>(&value)
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn full_sheet() -> serde_json::Value {
        json!({
            "vendor": "Enactic",
            "purchase_url": "https://openarm.dev/buy?variant=v2",
            "price": { "amount": 6500, "currency": "USD", "checked_on": "2026-10-31" },
            "availability": { "state": "preorder", "checked_on": "2026-10-30" },
            "mass_kg": 5.5,
            "height_m": 1.2,
        })
    }

    #[test]
    fn a_full_sheet_gives_each_field() {
        let parsed = sheet(full_sheet()).expect("parses");
        assert_eq!(parsed.vendor(), Some("Enactic"));
        assert_eq!(
            parsed.purchase_url(),
            Some("https://openarm.dev/buy?variant=v2")
        );
        let price = parsed.price().expect("a price");
        assert_eq!(
            (price.amount(), price.currency(), price.checked_on()),
            (6500.0, "USD", date("2026-10-31"))
        );
        let availability = parsed.availability().expect("an availability");
        assert_eq!(
            (availability.state(), availability.checked_on()),
            (AvailabilityState::Preorder, date("2026-10-30"))
        );
        assert_eq!(parsed.mass_kg(), Some(5.5));
        assert_eq!(parsed.height_m(), Some(1.2));
    }

    #[test]
    fn an_empty_sheet_gives_no_field() {
        let parsed = sheet(json!({})).expect("parses");
        assert_eq!(parsed, ProductSheet::empty());
        assert_eq!(parsed.vendor(), None);
        assert_eq!(parsed.purchase_url(), None);
        assert_eq!(parsed.price(), None);
        assert_eq!(parsed.availability(), None);
        assert_eq!(parsed.mass_kg(), None);
        assert_eq!(parsed.height_m(), None);
    }

    /// Each field alone, and each one that needs `purchase_url` with it.
    #[test]
    fn each_field_parses_alone() {
        assert_eq!(
            sheet(json!({ "vendor": "Hugging Face" })).unwrap().vendor(),
            Some("Hugging Face")
        );
        assert_eq!(
            sheet(json!({ "purchase_url": "HTTPS://Shop.Example.com" }))
                .unwrap()
                .purchase_url(),
            Some("https://shop.example.com/"),
            "the URL in its normal form"
        );
        assert_eq!(sheet(json!({ "mass_kg": 1 })).unwrap().mass_kg(), Some(1.0));
        assert_eq!(
            sheet(json!({ "height_m": 0.25 })).unwrap().height_m(),
            Some(0.25)
        );
        let url = "https://shop.example.com/so101";
        let priced = sheet(json!({ "purchase_url": url,
            "price": { "amount": 129.99, "currency": "EUR", "checked_on": "2024-02-29" } }))
        .unwrap();
        assert_eq!(priced.price().unwrap().amount(), 129.99);
        assert_eq!(priced.price().unwrap().currency(), "EUR");
        for (state, parsed) in [
            ("in_stock", AvailabilityState::InStock),
            ("preorder", AvailabilityState::Preorder),
            ("out_of_stock", AvailabilityState::OutOfStock),
            ("discontinued", AvailabilityState::Discontinued),
        ] {
            let available = sheet(json!({ "purchase_url": url,
                "availability": { "state": state, "checked_on": "2026-01-01" } }))
            .unwrap();
            assert_eq!(available.availability().unwrap().state(), parsed);
            assert_eq!(parsed.as_str(), state);
        }
    }

    #[test]
    fn each_refusal_names_its_field() {
        let url = "https://shop.example.com/so101";
        let price = |amount: serde_json::Value, currency: &str, checked_on: &str| {
            json!({ "purchase_url": url,
                    "price": { "amount": amount, "currency": currency, "checked_on": checked_on } })
        };
        for (value, refusal) in [
            (
                json!({ "purchase_url": "http://shop.example.com" }),
                "purchase_url: `http://shop.example.com` uses http, not https",
            ),
            (
                json!({ "purchase_url": "shop.example.com" }),
                "purchase_url: `shop.example.com` is not a URL (relative URL without a base)",
            ),
            (
                json!({ "purchase_url": "https://user:secret@shop.example.com" }),
                "purchase_url: `https://user:secret@shop.example.com` holds credentials, and \
                 the page of an offer is public",
            ),
            (
                price(json!(10), "USX", "2026-10-31"),
                "price.currency: `USX` is no ISO 4217 currency; write the active code of the \
                 currency in three capital letters, such as `USD` or `EUR`",
            ),
            (
                price(json!(10), "usd", "2026-10-31"),
                "price.currency: `usd` is no ISO 4217 currency",
            ),
            (
                price(json!(10), "HRK", "2026-10-31"),
                "price.currency: `HRK` is no ISO 4217 currency",
            ),
            (
                price(json!(0), "USD", "2026-10-31"),
                "price.amount: `0` is 0 or less, and the value is more than 0",
            ),
            (
                price(json!(-5.5), "USD", "2026-10-31"),
                "price.amount: `-5.5` is 0 or less",
            ),
            (
                price(json!("10"), "USD", "2026-10-31"),
                "price.amount: invalid type: string \"10\", expected f64",
            ),
            (
                price(json!(10), "USD", "2026-1-31"),
                "price.checked_on: `2026-1-31` is no calendar date written YYYY-MM-DD",
            ),
            (
                price(json!(10), "USD", "2026-02-30"),
                "price.checked_on: `2026-02-30` is no calendar date written YYYY-MM-DD",
            ),
            (
                price(json!(10), "USD", "31/10/2026"),
                "price.checked_on: `31/10/2026` is no calendar date",
            ),
            (
                price(json!(10), "USD", "2026-10-31T00:00:00Z"),
                "price.checked_on: `2026-10-31T00:00:00Z` is no calendar date",
            ),
            (
                json!({ "purchase_url": url, "price": { "amount": 10, "currency": "USD" } }),
                "price: missing field `checked_on`",
            ),
            (
                json!({ "purchase_url": url,
                        "price": { "amount": 10, "currency": "USD", "checked_on": "2026-10-31",
                                   "tax": 0 } }),
                "price.tax: unknown field `tax`, expected one of `amount`, `currency`, `checked_on`",
            ),
            (
                json!({ "purchase_url": url,
                        "availability": { "state": "soon", "checked_on": "2026-10-31" } }),
                "availability.state: unknown variant `soon`, expected one of `in_stock`, \
                 `preorder`, `out_of_stock`, `discontinued`",
            ),
            (
                json!({ "purchase_url": url, "availability": { "state": "in_stock" } }),
                "availability: missing field `checked_on`",
            ),
            (json!({ "mass_kg": 0 }), "mass_kg: `0` is 0 or less"),
            (json!({ "height_m": -1.2 }), "height_m: `-1.2` is 0 or less"),
            (
                json!({ "vendor": "  " }),
                "vendor: the vendor is empty; leave `vendor` out of a sheet that names no seller",
            ),
            (
                json!({ "vendor": "Enactic\nInc." }),
                "vendor: the vendor is one line, with no line break or other control character",
            ),
            (
                json!({ "vendor": "é".repeat(MAX_DESCRIPTION_CHARS + 1) }),
                "vendor: the vendor is at most 200 characters, and this one has 201",
            ),
            (
                json!({ "price": { "amount": 10, "currency": "USD", "checked_on": "2026-10-31" } }),
                "`price` is a fact of the offer that `purchase_url` opens, so a sheet that gives \
                 `price` gives `purchase_url`",
            ),
            (
                json!({ "availability": { "state": "in_stock", "checked_on": "2026-10-31" } }),
                "`availability` is a fact of the offer that `purchase_url` opens, so a sheet \
                 that gives `availability` gives `purchase_url`",
            ),
            (
                json!({ "colour": "red" }),
                "colour: unknown field `colour`, expected one of `vendor`, `purchase_url`, \
                 `price`, `availability`, `mass_kg`, `height_m`",
            ),
            (
                json!("Enactic"),
                "invalid type: string \"Enactic\", expected a product sheet object",
            ),
        ] {
            let error = sheet(value.clone()).expect_err("the sheet is refused");
            assert!(error.starts_with(refusal), "{value}: {error}");
        }
    }

    #[test]
    fn a_number_that_is_not_finite_is_refused() {
        for value in [f64::INFINITY, f64::NAN, f64::NEG_INFINITY] {
            let error = PositiveNumber::try_from(value).expect_err("not finite");
            assert!(error.ends_with("is not a finite number"), "{error}");
        }
    }

    /// A date of the sheet is refused when it is after the date it is held
    /// to, and a date of today or before holds.
    #[test]
    fn a_date_after_today_is_refused_with_its_field() {
        let parsed = sheet(full_sheet()).expect("parses");
        assert!(parsed.dates_after(date("2026-10-31")).is_empty());
        assert!(parsed.dates_after(date("2027-01-01")).is_empty());
        assert_eq!(
            parsed.dates_after(date("2026-10-30")),
            [
                "price.checked_on: `2026-10-31` is in the future: the latest date on Earth is \
              `2026-10-30`, and a `checked_on` is the date of a check that took place"
            ]
        );
        assert_eq!(parsed.dates_after(date("2026-10-29")).len(), 2);
        assert!(
            ProductSheet::empty()
                .dates_after(date("2000-01-01"))
                .is_empty()
        );
    }
}
