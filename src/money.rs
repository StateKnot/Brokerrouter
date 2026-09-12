use crate::error::{Error, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub fn nonnegative(value: &str) -> Result<Decimal> {
    let n = Decimal::from_str(value).map_err(|_| Error::Invalid("invalid decimal"))?;
    if n.is_sign_negative() || n.scale() > 12 || n > Decimal::from(1_000_000_000u64) {
        return Err(Error::Invalid(
            "decimal must be nonnegative, <= 1e9 and <= 12 decimal places",
        ));
    }
    Ok(n)
}

pub fn cny_micros(value: &str) -> Result<i64> {
    let n = nonnegative(value)?;
    if n.scale() > 6 {
        return Err(Error::Invalid(
            "CNY budgets support at most 6 decimal places",
        ));
    }
    to_micros(n)
}

fn to_micros(cny: Decimal) -> Result<i64> {
    let cny = cny.normalize();
    let n = cny.mantissa();
    if n < 0 {
        return Err(Error::Invalid("negative cost"));
    }
    let rounded = if cny.scale() <= 6 {
        n.checked_mul(10i128.pow(6 - cny.scale()))
    } else {
        let divisor = 10i128.pow(cny.scale() - 6);
        n.checked_add(divisor - 1).map(|n| n / divisor)
    };
    rounded
        .and_then(|n| i64::try_from(n).ok())
        .ok_or(Error::Invalid("amount exceeds supported range"))
}

// Decimal checked arithmetic may reduce scale to fit 96 bits. Money must reject
// that loss, not round before FX and final CNY micro-unit settlement.
fn exact_parts(mut mantissa: i128, mut scale: u32) -> Result<Decimal> {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    Decimal::try_from_i128_with_scale(mantissa, scale)
        .map_err(|_| Error::Invalid("exact price precision exceeds supported range"))
}
fn exact_mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (a, b) = (a.normalize(), b.normalize());
    let n = a
        .mantissa()
        .checked_mul(b.mantissa())
        .ok_or(Error::Invalid("price calculation overflow"))?;
    exact_parts(n, a.scale() + b.scale())
}
fn exact_add(a: Decimal, b: Decimal) -> Result<Decimal> {
    let (a, b) = (a.normalize(), b.normalize());
    let scale = a.scale().max(b.scale());
    let n = a
        .mantissa()
        .checked_mul(10i128.pow(scale - a.scale()))
        .zip(b.mantissa().checked_mul(10i128.pow(scale - b.scale())))
        .and_then(|(a, b)| a.checked_add(b))
        .ok_or(Error::Invalid("price calculation overflow"))?;
    exact_parts(n, scale)
}

pub fn format_micros(micros: i64) -> String {
    format!("{:.6}", Decimal::from_i128_with_scale(micros as i128, 6))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceSnapshot {
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub input_per_million: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub output_per_million: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cny_per_unit: Decimal,
    pub fx_version: Option<i64>,
}

impl PriceSnapshot {
    pub fn validate(&self) -> Result<()> {
        nonnegative(&self.input_per_million.to_string())?;
        nonnegative(&self.output_per_million.to_string())?;
        validate_currency(&self.currency, self.cny_per_unit, self.fx_version)
    }

    pub fn breakdown(&self, input_tokens: u64, output_tokens: u64) -> Result<CostBreakdown> {
        self.validate()?;
        let input = exact_mul(self.input_per_million, Decimal::from(input_tokens))?;
        let output = exact_mul(self.output_per_million, Decimal::from(output_tokens))?;
        let sum = exact_add(input, output)?;
        let original_amount = exact_parts(sum.mantissa(), sum.scale() + 6)?;
        breakdown(&self.currency, original_amount, self.cny_per_unit)
    }

    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> Result<i64> {
        Ok(self.breakdown(input_tokens, output_tokens)?.cny_micros)
    }
}

fn validate_currency(currency: &str, cny_per_unit: Decimal, fx_version: Option<i64>) -> Result<()> {
    match currency {
        "CNY" if cny_per_unit == Decimal::ONE && fx_version.is_none() => Ok(()),
        "USD"
            if cny_per_unit > Decimal::ZERO
                && cny_per_unit <= Decimal::from(1_000_000)
                && cny_per_unit.scale() <= 10
                && fx_version.is_some_and(|id| id > 0) =>
        {
            Ok(())
        }
        _ => Err(Error::Invalid("invalid currency or exchange-rate snapshot")),
    }
}

fn breakdown(
    currency: &str,
    original_amount: Decimal,
    cny_per_unit: Decimal,
) -> Result<CostBreakdown> {
    let cny_exact = exact_mul(original_amount, cny_per_unit)?;
    Ok(CostBreakdown {
        original_currency: currency.into(),
        original_amount,
        cny_exact,
        cny_micros: to_micros(cny_exact)?,
    })
}

/// A frozen non-token quote. Unit labels must include variant distinctions such as
/// resolution when they affect price; they are never inferred from a model name.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitPriceSnapshot {
    pub schema: String,
    pub price_version: uuid::Uuid,
    pub currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub cny_per_unit: Decimal,
    pub fx_version: Option<i64>,
    pub rates: Vec<UnitRate>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitRate {
    pub unit: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price_per_unit: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub maximum_quantity: Decimal,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitUsage {
    pub unit: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
}
impl UnitPriceSnapshot {
    pub fn validate(&self) -> Result<()> {
        if self.schema != "units-v1"
            || self.price_version.is_nil()
            || !(1..=16).contains(&self.rates.len())
        {
            return Err(Error::Invalid("invalid unit pricing schema"));
        }
        validate_currency(&self.currency, self.cny_per_unit, self.fx_version)?;
        let mut seen = std::collections::HashSet::new();
        for rate in &self.rates {
            if rate.unit.is_empty()
                || rate.unit.len() > 64
                || !rate
                    .unit
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-.".contains(&c))
                || !seen.insert(&rate.unit)
                || rate.maximum_quantity <= Decimal::ZERO
            {
                return Err(Error::Invalid("invalid or duplicate billing unit"));
            }
            nonnegative(&rate.price_per_unit.to_string())?;
            nonnegative(&rate.maximum_quantity.to_string())?;
        }
        Ok(())
    }
    pub fn maximum_cost(&self) -> Result<i64> {
        let usage: Vec<_> = self
            .rates
            .iter()
            .map(|r| UnitUsage {
                unit: r.unit.clone(),
                quantity: r.maximum_quantity,
            })
            .collect();
        Ok(self.breakdown(&usage)?.cny_micros)
    }
    pub fn breakdown(&self, usage: &[UnitUsage]) -> Result<CostBreakdown> {
        self.validate()?;
        // Every quoted dimension is explicit, including zero use. A missing provider
        // usage field is not silently converted to a free generation.
        if usage.len() != self.rates.len() {
            return Err(Error::Invalid("all quoted usage dimensions are required"));
        }
        let mut seen = std::collections::HashSet::new();
        let mut amount = Decimal::ZERO;
        for used in usage {
            let rate = self
                .rates
                .iter()
                .find(|r| r.unit == used.unit)
                .ok_or(Error::Invalid("unquoted billing unit"))?;
            nonnegative(&used.quantity.to_string())?;
            if !seen.insert(&used.unit) || used.quantity > rate.maximum_quantity {
                return Err(Error::Invalid(
                    "usage exceeds quote or repeats a unit; reconciliation required",
                ));
            }
            amount = exact_add(amount, exact_mul(rate.price_per_unit, used.quantity)?)?;
        }
        breakdown(&self.currency, amount, self.cny_per_unit)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub original_currency: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub original_amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cny_exact: Decimal,
    pub cny_micros: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cny_is_canonical_and_sub_micro_unit_prices_are_preserved() {
        let p = PriceSnapshot {
            currency: "CNY".into(),
            input_per_million: nonnegative("0.1").unwrap(),
            output_per_million: Decimal::ZERO,
            cny_per_unit: Decimal::ONE,
            fx_version: None,
        };
        assert_eq!(p.cost(1_000_000, 0).unwrap(), 100_000);
        assert_eq!(p.cost(1, 0).unwrap(), 1);
        let usd = PriceSnapshot {
            currency: "USD".into(),
            cny_per_unit: nonnegative("7.123456789").unwrap(),
            fx_version: Some(1),
            ..p
        };
        assert_eq!(usd.cost(1_000_000, 0).unwrap(), 712_346);
        let bill = usd.breakdown(1_000_000, 0).unwrap();
        assert_eq!(bill.original_amount, nonnegative("0.1").unwrap());
        assert_eq!(bill.cny_exact, nonnegative("0.7123456789").unwrap());
        assert_eq!(bill.cny_micros, 712_346);
        assert_eq!(usd.cost(0, 0).unwrap(), 0);
        assert!(
            PriceSnapshot {
                currency: "EUR".into(),
                ..usd.clone()
            }
            .cost(1, 0)
            .is_err()
        );
        assert!(
            PriceSnapshot {
                fx_version: None,
                ..usd
            }
            .cost(1, 0)
            .is_err()
        );
        assert_eq!(cny_micros("12.345678").unwrap(), 12_345_678);
        assert!(cny_micros("0.0000001").is_err());
        assert!(nonnegative("-1").is_err());
        assert_eq!(format_micros(1), "0.000001");
    }
}
