use crate::error::{Error, Result};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
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
    cny.checked_mul(Decimal::from(1_000_000))
        .and_then(|v| {
            v.round_dp_with_strategy(0, RoundingStrategy::ToPositiveInfinity)
                .to_i64()
        })
        .ok_or(Error::Invalid("amount exceeds supported range"))
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
        match self.currency.as_str() {
            "CNY" if self.cny_per_unit == Decimal::ONE && self.fx_version.is_none() => Ok(()),
            "USD"
                if self.cny_per_unit > Decimal::ZERO
                    && self.cny_per_unit <= Decimal::from(1_000_000)
                    && self.cny_per_unit.scale() <= 10
                    && self.fx_version.is_some_and(|id| id > 0) =>
            {
                Ok(())
            }
            _ => Err(Error::Invalid("invalid currency or exchange-rate snapshot")),
        }
    }

    pub fn breakdown(&self, input_tokens: u64, output_tokens: u64) -> Result<CostBreakdown> {
        self.validate()?;
        let input = self
            .input_per_million
            .checked_mul(Decimal::from(input_tokens));
        let output = self
            .output_per_million
            .checked_mul(Decimal::from(output_tokens));
        let original_amount = input
            .zip(output)
            .and_then(|(a, b)| a.checked_add(b))
            .and_then(|v| v.checked_div(Decimal::from(1_000_000)))
            .ok_or(Error::Invalid("price calculation overflow"))?;
        let cny_exact = original_amount
            .checked_mul(self.cny_per_unit)
            .ok_or(Error::Invalid("price calculation overflow"))?;
        Ok(CostBreakdown {
            original_currency: self.currency.clone(),
            original_amount,
            cny_exact,
            cny_micros: to_micros(cny_exact)?,
        })
    }

    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> Result<i64> {
        Ok(self.breakdown(input_tokens, output_tokens)?.cny_micros)
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
