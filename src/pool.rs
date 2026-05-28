use serde::Serialize;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::IdentityConfig;

#[derive(Clone)]
pub struct Identity {
    pub id: String,
    pub token: String,
}

struct RateState {
    remaining: Option<u32>,
    reset_at: Option<u64>,
    request_count: u64,
}

pub struct PatPool {
    identities: Vec<Identity>,
    states: Mutex<Vec<RateState>>,
}

impl PatPool {
    pub fn new(configs: &[IdentityConfig]) -> Self {
        let identities: Vec<Identity> = configs
            .iter()
            .map(|c| Identity { id: c.id.clone(), token: c.token.clone() })
            .collect();
        let states = configs.iter().map(|_| RateState {
            remaining: None,
            reset_at: None,
            request_count: 0,
        }).collect();
        Self { identities, states: Mutex::new(states) }
    }

    /// Select the identity with the most remaining rate limit budget.
    pub fn select(&self) -> Result<Identity, &'static str> {
        if self.identities.is_empty() {
            return Err("no identities configured");
        }
        let mut states = self.states.lock().unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        let mut best_idx = 0;
        let mut best_score: i64 = i64::MIN;

        for (i, state) in states.iter().enumerate() {
            let remaining = match (state.remaining, state.reset_at) {
                (Some(r), Some(reset)) if reset > now => r as i64,
                _ => 5000, // assume full budget if unknown or expired
            };
            // Tie-break by least used
            let score = remaining * 1000 - state.request_count as i64;
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }

        states[best_idx].request_count += 1;
        Ok(self.identities[best_idx].clone())
    }

    pub fn update_rate(&self, id: &str, remaining: Option<u32>, reset_at: Option<u64>) {
        let Some(idx) = self.identities.iter().position(|i| i.id == id) else { return };
        let mut states = self.states.lock().unwrap();
        if let Some(r) = remaining {
            states[idx].remaining = Some(r);
        }
        if let Some(reset) = reset_at {
            states[idx].reset_at = Some(reset);
        }
    }

    pub fn snapshot(&self) -> Vec<IdentitySnapshot> {
        let states = self.states.lock().unwrap();
        self.identities.iter().enumerate().map(|(i, ident)| {
            let state = &states[i];
            IdentitySnapshot {
                id: ident.id.clone(),
                remaining: state.remaining,
                reset_at: state.reset_at,
                request_count: state.request_count,
            }
        }).collect()
    }
}

#[derive(Serialize)]
pub struct IdentitySnapshot {
    pub id: String,
    pub remaining: Option<u32>,
    pub reset_at: Option<u64>,
    pub request_count: u64,
}
