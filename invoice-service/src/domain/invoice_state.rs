//! The invoice state machine (decisions.md D1).
//!
//! ```text
//!             finalize                payment succeeded
//!   draft ─────────────▶ open ─────────────────────────▶ paid      [terminal]
//!     │                   │
//!     │ void              │ mark_uncollectible
//!     ▼                   ▼
//!    void ◀──── void ── uncollectible ──payment succeeded (late)──▶ paid
//!  [terminal]  (open → void too, unless a payment attempt is pending)
//! ```
//!
//! A failed payment is NOT a transition: the attempt is marked failed, the invoice stays
//! payable so the customer can try another card.

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceState {
    Draft,
    Open,
    Paid,
    Void,
    Uncollectible,
}

use InvoiceState::*;

impl InvoiceState {
    pub const ALL: [InvoiceState; 5] = [Draft, Open, Paid, Void, Uncollectible];

    /// The single source of truth for which transitions are allowed.
    pub fn can_transition_to(self, to: InvoiceState) -> bool {
        matches!(
            (self, to),
            (Draft, Open)
                | (Draft, Void)
                | (Open, Paid)
                | (Open, Void)
                | (Open, Uncollectible)
                | (Uncollectible, Paid)
                | (Uncollectible, Void)
        )
    }

    pub fn is_terminal(self) -> bool {
        Self::ALL.iter().all(|&to| !self.can_transition_to(to))
    }

    /// An invoice can be paid exactly when it can move to `paid`.
    pub fn is_payable(self) -> bool {
        self.can_transition_to(Paid)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Draft => "draft",
            Open => "open",
            Paid => "paid",
            Void => "void",
            Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_documented_transitions_are_allowed() {
        let allowed: Vec<(InvoiceState, InvoiceState)> = InvoiceState::ALL
            .into_iter()
            .flat_map(|from| InvoiceState::ALL.into_iter().map(move |to| (from, to)))
            .filter(|&(from, to)| from.can_transition_to(to))
            .collect();
        assert_eq!(
            allowed,
            vec![
                (Draft, Open),
                (Draft, Void),
                (Open, Paid),
                (Open, Void),
                (Open, Uncollectible),
                (Uncollectible, Paid),
                (Uncollectible, Void),
            ]
        );
    }

    #[test]
    fn paid_and_void_are_the_only_terminal_states() {
        let terminal: Vec<_> = InvoiceState::ALL.into_iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(terminal, vec![Paid, Void]);
    }

    #[test]
    fn only_open_and_uncollectible_are_payable() {
        let payable: Vec<_> = InvoiceState::ALL.into_iter().filter(|s| s.is_payable()).collect();
        assert_eq!(payable, vec![Open, Uncollectible]);
    }

    #[test]
    fn nothing_returns_to_draft() {
        assert!(InvoiceState::ALL.iter().all(|s| !s.can_transition_to(Draft)));
    }

    #[test]
    fn parse_round_trips() {
        for s in InvoiceState::ALL {
            assert_eq!(InvoiceState::parse(s.as_str()), Some(s));
        }
        assert_eq!(InvoiceState::parse("refunded"), None);
    }
}
