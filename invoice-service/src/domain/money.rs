//! Money math. All amounts are `i64` cents; every multiplication and addition is checked,
//! so an overflow is a validation error instead of a silently wrapped (or panicking) number.
//! There is no floating point anywhere in this module, or in the money path at all.

use serde::Deserialize;

pub const MAX_LINE_ITEMS: usize = 100;

#[derive(Deserialize)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

/// A validated line item with its server-computed amount.
pub struct PricedLineItem<'a> {
    pub description: &'a str,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

/// Validates line items and computes each line amount and the invoice total.
/// The client never supplies a total; this function is the only place one is produced.
pub fn price_line_items(items: &[LineItemInput]) -> Result<(Vec<PricedLineItem<'_>>, i64), String> {
    if items.is_empty() {
        return Err("an invoice needs at least one line item".into());
    }
    if items.len() > MAX_LINE_ITEMS {
        return Err(format!("an invoice can have at most {MAX_LINE_ITEMS} line items"));
    }

    let mut priced = Vec::with_capacity(items.len());
    let mut total: i64 = 0;
    for (i, item) in items.iter().enumerate() {
        let description = item.description.trim();
        if description.is_empty() {
            return Err(format!("line_items[{i}].description must not be empty"));
        }
        if item.quantity <= 0 {
            return Err(format!("line_items[{i}].quantity must be greater than 0"));
        }
        if item.unit_amount_cents < 0 {
            return Err(format!("line_items[{i}].unit_amount_cents must not be negative"));
        }
        let amount = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(|| format!("line_items[{i}] amount is too large"))?;
        total = total.checked_add(amount).ok_or("invoice total is too large")?;
        priced.push(PricedLineItem {
            description,
            quantity: item.quantity,
            unit_amount_cents: item.unit_amount_cents,
            amount_cents: amount,
        });
    }

    if total <= 0 {
        return Err("invoice total must be greater than 0".into());
    }
    Ok((priced, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i64, unit_amount_cents: i64) -> LineItemInput {
        LineItemInput { description: "thing".into(), quantity, unit_amount_cents }
    }

    #[test]
    fn computes_line_amounts_and_total() {
        let items = [item(3, 1999), item(1, 500)];
        let (priced, total) = price_line_items(&items).unwrap();
        assert_eq!(priced[0].amount_cents, 5997);
        assert_eq!(priced[1].amount_cents, 500);
        assert_eq!(total, 6497);
    }

    #[test]
    fn rejects_overflow_instead_of_wrapping() {
        assert!(price_line_items(&[item(i64::MAX, 2)]).is_err());
        assert!(price_line_items(&[item(1, i64::MAX), item(1, 1)]).is_err());
    }

    #[test]
    fn rejects_bad_quantities_and_amounts() {
        assert!(price_line_items(&[item(0, 100)]).is_err());
        assert!(price_line_items(&[item(-1, 100)]).is_err());
        assert!(price_line_items(&[item(1, -100)]).is_err());
    }

    #[test]
    fn free_items_allowed_but_not_a_zero_invoice() {
        assert!(price_line_items(&[item(1, 0), item(1, 100)]).is_ok());
        assert!(price_line_items(&[item(1, 0)]).is_err());
    }

    #[test]
    fn rejects_empty_and_too_many_items() {
        assert!(price_line_items(&[]).is_err());
        let many: Vec<_> = (0..=MAX_LINE_ITEMS).map(|_| item(1, 1)).collect();
        assert!(price_line_items(&many).is_err());
    }
}
