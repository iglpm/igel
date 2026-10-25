//! Job tokens without starvation: jobs start when the scheduler says and owe a jobserver token that a
//! background thread reclaims, so the critical path never waits behind other crates' codegen.

use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct Ledger {
    owed: usize,
    paid: usize,
    reclaiming: bool,
}

#[derive(Clone)]
pub struct JobTokens {
    client: jobserver::Client,
    ledger: Arc<(Mutex<Ledger>, Condvar)>,
}

pub struct JobToken {
    tokens: JobTokens,
}

impl JobTokens {
    pub fn new(client: jobserver::Client) -> Self {
        Self {
            client,
            ledger: Arc::default(),
        }
    }

    pub fn start(&self) -> JobToken {
        // A shared pipe can't be read without blocking, so jobs start owing and a thread takes the token.
        let (lock, _) = &*self.ledger;
        let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
        ledger.owed += 1;
        if !ledger.reclaiming {
            ledger.reclaiming = true;
            let me = self.clone();
            std::thread::spawn(move || me.reclaim());
        }
        JobToken {
            tokens: self.clone(),
        }
    }

    fn reclaim(self) {
        let (lock, _) = &*self.ledger;
        loop {
            {
                let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
                if ledger.owed == 0 {
                    ledger.reclaiming = false;
                    return;
                }
            }
            if self.client.acquire_raw().is_err() {
                let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
                ledger.reclaiming = false;
                return;
            }
            let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
            if ledger.owed > 0 {
                ledger.owed -= 1;
                ledger.paid += 1;
            } else {
                let _ = self.client.release_raw();
            }
        }
    }

    pub fn owed(&self) -> usize {
        self.ledger.0.lock().map(|l| l.owed).unwrap_or(0)
    }
}

impl Drop for JobToken {
    fn drop(&mut self) {
        let (lock, _) = &*self.tokens.ledger;
        let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
        if ledger.owed > 0 {
            ledger.owed -= 1;
        } else if ledger.paid > 0 {
            ledger.paid -= 1;
            let _ = self.tokens.client.release_raw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_starts_even_when_every_token_is_lent_out() {
        let client = jobserver::Client::new(2).unwrap();
        let tokens = JobTokens::new(client.clone());
        let wait_owed = |n: usize| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while tokens.owed() != n && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            tokens.owed()
        };
        let a = tokens.start();
        let b = tokens.start();
        assert_eq!(wait_owed(0), 0);
        let c = tokens.start();
        assert_eq!(wait_owed(1), 1);
        drop(a);
        assert_eq!(wait_owed(0), 0);
        drop(b);
        drop(c);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while client.available().unwrap() != 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(client.available().unwrap(), 2);
    }
}
