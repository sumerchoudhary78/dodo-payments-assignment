//! Charge model and the token → outcome rules from the assignment spec.

use serde::Serialize;
use uuid::Uuid;

/// What the PSP reports about a charge. Serialized as `{"status": "...", ...}`.
#[derive(Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Charge {
    Processing,
    Succeeded { psp_ref: Uuid },
    Failed { code: String },
}

/// How the mock should behave for a given card token.
#[derive(Clone, Copy)]
pub enum Outcome {
    Succeed,
    Decline(&'static str),
    SlowSucceed,
    NetworkError,
}

impl Outcome {
    pub fn from_token(token: &str) -> Self {
        match token {
            "tok_success" => Outcome::Succeed,
            "tok_insufficient_funds" => Outcome::Decline("insufficient_funds"),
            "tok_card_declined" => Outcome::Decline("card_declined"),
            "tok_timeout" => Outcome::SlowSucceed,
            "tok_network_error" => Outcome::NetworkError,
            _ => Outcome::Decline("invalid_card_token"),
        }
    }

    /// The final state of the charge once processing finishes.
    /// A network error still records a failed charge: the PSP knows what happened,
    /// even though the caller only ever sees a 500.
    pub fn final_charge(self) -> Charge {
        match self {
            Outcome::Succeed | Outcome::SlowSucceed => Charge::Succeeded { psp_ref: Uuid::new_v4() },
            Outcome::Decline(code) => Charge::Failed { code: code.into() },
            Outcome::NetworkError => Charge::Failed { code: "processing_error".into() },
        }
    }
}
