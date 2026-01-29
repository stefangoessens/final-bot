#![allow(dead_code)]

use crate::error::BotResult;
use crate::state::inventory::TokenSide;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedeemRequest {
    pub condition_id: String,
    pub outcome: TokenSide,
    pub qty: u64,
}

pub trait RedeemClient {
    fn redeem(&self, request: &RedeemRequest) -> BotResult<()>;
}

#[derive(Debug, Clone)]
pub struct Redeemer<C> {
    client: C,
}

impl<C: RedeemClient> Redeemer<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    pub fn redeem(&self, request: &RedeemRequest) -> BotResult<()> {
        self.client.redeem(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockRedeemClient {
        calls: Arc<Mutex<Vec<RedeemRequest>>>,
    }

    impl RedeemClient for MockRedeemClient {
        fn redeem(&self, request: &RedeemRequest) -> BotResult<()> {
            self.calls.lock().expect("lock calls").push(request.clone());
            Ok(())
        }
    }

    #[test]
    fn redeemer_invokes_client() {
        let client = MockRedeemClient::default();
        let redeemer = Redeemer::new(client.clone());

        let request = RedeemRequest {
            condition_id: "cond".to_string(),
            outcome: TokenSide::Up,
            qty: 10,
        };

        redeemer.redeem(&request).expect("redeem call");

        let calls = client.calls.lock().expect("lock calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], request);
    }
}
