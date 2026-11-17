use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// ISO 4217 currency code, e.g. `EUR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Currency([u8; 3]);

impl Currency {
    pub const EUR: Currency = Currency(*b"EUR");

    pub fn as_str(&self) -> &str {
        // Constructed only from ASCII uppercase letters.
        std::str::from_utf8(&self.0).expect("currency is ASCII")
    }
}

impl TryFrom<String> for Currency {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl std::str::FromStr for Currency {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bytes: [u8; 3] = s
            .as_bytes()
            .try_into()
            .map_err(|_| Error::Validation(format!("invalid currency code: {s}")))?;
        if !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(Error::Validation(format!("invalid currency code: {s}")));
        }
        Ok(Currency(bytes))
    }
}

impl From<Currency> for String {
    fn from(value: Currency) -> Self {
        value.as_str().to_string()
    }
}

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An amount in a currency. Never use floats for money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub currency: Currency,
}

impl Money {
    pub fn new(amount: Decimal, currency: Currency) -> Self {
        Self { amount, currency }
    }

    /// Adds two amounts. Fails if the currencies differ.
    pub fn checked_add(self, other: Money) -> Result<Money> {
        if self.currency != other.currency {
            return Err(Error::Validation(format!(
                "cannot add {} to {}",
                other.currency, self.currency
            )));
        }
        let amount = self
            .amount
            .checked_add(other.amount)
            .ok_or_else(|| Error::Validation("amount overflow".into()))?;
        Ok(Money::new(amount, self.currency))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::dec;

    #[test]
    fn adds_same_currency() {
        let a = Money::new(dec!(10.50), Currency::EUR);
        let b = Money::new(dec!(0.25), Currency::EUR);
        assert_eq!(a.checked_add(b).unwrap().amount, dec!(10.75));
    }

    #[test]
    fn rejects_mixed_currencies() {
        let a = Money::new(dec!(1), Currency::EUR);
        let b = Money::new(dec!(1), "USD".parse().unwrap());
        assert!(a.checked_add(b).is_err());
    }

    #[test]
    fn rejects_bad_currency_codes() {
        assert!("eur".parse::<Currency>().is_err());
        assert!("EURO".parse::<Currency>().is_err());
    }

    #[test]
    fn serializes_amount_as_string() {
        let m = Money::new(dec!(19.99), Currency::EUR);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"amount":"19.99","currency":"EUR"}"#);
    }
}
