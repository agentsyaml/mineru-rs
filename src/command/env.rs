use crate::OfficialPdfOptions;
use std::{ffi::OsString, time::Duration};

#[derive(Debug, PartialEq, Eq)]
pub enum Decimal {
    Invalid,
    NonPositive,
    Positive(u64),
}

fn lex_decimal(value: &OsString, max: u64) -> Option<(bool, bool, u64)> {
    let value = value.to_str()?.trim();
    let (negative, digits) = match value.strip_prefix('-') {
        Some(v) => (true, v),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    if digits.is_empty() {
        return None;
    }
    let mut number = 0u64;
    let mut nonzero = false;
    let mut previous_digit = false;
    for c in digits.chars() {
        if let Some(digit) = c.to_digit(10) {
            nonzero |= digit != 0;
            number = number
                .saturating_mul(10)
                .saturating_add(digit as u64)
                .min(max);
            previous_digit = true;
        } else if c == '_' && previous_digit {
            previous_digit = false;
        } else {
            return None;
        }
    }
    previous_digit.then_some((negative, nonzero, number))
}

pub fn decimal(value: &OsString, max: u64) -> Decimal {
    let Some((negative, nonzero, number)) = lex_decimal(value, max) else {
        return Decimal::Invalid;
    };
    if negative || !nonzero {
        Decimal::NonPositive
    } else {
        Decimal::Positive(number)
    }
}

pub fn official_page_concurrency(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Result<usize, &'static str> {
    match lookup("MINERU_OFFICIAL_PAGE_CONCURRENCY") {
        None => Ok(4),
        Some(value) => match decimal(&value, u64::MAX) {
            Decimal::Positive(value @ 1..=8) => Ok(value as usize),
            Decimal::Invalid | Decimal::NonPositive | Decimal::Positive(_) => {
                Err("MINERU_OFFICIAL_PAGE_CONCURRENCY must be an integer from 1 to 8")
            }
        },
    }
}

#[allow(dead_code)] // The API binary uses this; the canonical CLI compiles the shared module too.
pub fn nonnegative_decimal(value: &OsString, max: u64) -> Option<u64> {
    let (negative, nonzero, number) = lex_decimal(value, max)?;
    (!negative || !nonzero).then_some(number)
}

/// Applies the P3a route environment without reading or mutating process state.
pub fn apply_route_env(
    route: &mut OfficialPdfOptions,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> bool {
    let processing_invalid = match lookup("MINERU_PROCESSING_WINDOW_SIZE") {
        None => false,
        Some(v) => match decimal(&v, usize::MAX as u64) {
            Decimal::Invalid => {
                route.processing_window_size = 64;
                true
            }
            Decimal::NonPositive => {
                route.processing_window_size = 1;
                false
            }
            Decimal::Positive(v) => {
                route.processing_window_size = v as usize;
                false
            }
        },
    };
    if let Some(v) = lookup("MINERU_PDF_RENDER_THREADS") {
        match decimal(&v, usize::MAX as u64) {
            Decimal::Positive(v) => route.render_workers = v as usize,
            Decimal::Invalid | Decimal::NonPositive => route.render_workers = 3,
        }
    }
    if let Some(v) = lookup("MINERU_PDF_RENDER_TIMEOUT") {
        match decimal(&v, u64::MAX) {
            Decimal::Positive(v) => route.render_timeout = Duration::from_secs(v),
            Decimal::Invalid | Decimal::NonPositive => {
                route.render_timeout = Duration::from_secs(300)
            }
        }
    }
    for (name, target) in [
        ("MINERU_FORMULA_ENABLE", &mut route.formula_enable),
        ("MINERU_TABLE_ENABLE", &mut route.table_enable),
    ] {
        if let Some(v) = lookup(name) {
            *target = v.to_str().is_some_and(|v| v.to_lowercase() == "true");
        }
    }
    processing_invalid
}

#[allow(dead_code)] // This source is also included by binaries that only need apply_route_env.
#[derive(Clone)]
pub struct RouteEnv {
    pub route: OfficialPdfOptions,
    pub formula: Option<bool>,
    pub table: Option<bool>,
}
#[allow(dead_code)] // The API binary uses this; the canonical CLI compiles the shared module too.
pub fn snapshot_route_env(lookup: impl Fn(&str) -> Option<OsString>) -> RouteEnv {
    let formula = lookup("MINERU_FORMULA_ENABLE")
        .map(|v| v.to_str().is_some_and(|v| v.to_lowercase() == "true"));
    let table = lookup("MINERU_TABLE_ENABLE")
        .map(|v| v.to_str().is_some_and(|v| v.to_lowercase() == "true"));
    let mut route = OfficialPdfOptions::default();
    apply_route_env(&mut route, lookup);
    RouteEnv {
        route,
        formula,
        table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_preserves_negative_classification_while_nonnegative_keeps_zero() {
        assert_eq!(decimal(&" -0 ".into(), 9), Decimal::NonPositive);
        assert_eq!(decimal(&"-2".into(), 9), Decimal::NonPositive);
        assert_eq!(decimal(&"+1_2".into(), 9), Decimal::Positive(9));
        assert_eq!(nonnegative_decimal(&"0".into(), 9), Some(0));
        assert_eq!(nonnegative_decimal(&"-0".into(), 9), Some(0));
        assert_eq!(nonnegative_decimal(&"-2".into(), 9), None);
        assert_eq!(nonnegative_decimal(&"1__2".into(), 9), None);
        assert_eq!(nonnegative_decimal(&"999".into(), 9), Some(9));
    }

    #[test]
    fn official_page_concurrency_is_strict_and_defaults_to_four() {
        assert_eq!(official_page_concurrency(|_| None), Ok(4));
        assert_eq!(official_page_concurrency(|_| Some("4".into())), Ok(4));
        for value in ["0", "9", "bad", "1_0", "-1"] {
            assert!(official_page_concurrency(|_| Some(value.into())).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn official_page_concurrency_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        assert!(official_page_concurrency(|_| Some(OsString::from_vec(vec![0xff]))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn nonnegative_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        assert_eq!(
            nonnegative_decimal(&OsString::from_vec(vec![0xff]), 9),
            None
        );
    }
}
