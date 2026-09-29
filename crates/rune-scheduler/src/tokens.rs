//! Job tokens without starvation.
//!
//! Every compiler Rune starts shares one GNU jobserver, so a `rustc` can
//! borrow idle cores for its codegen threads. If a new job also had to take
//! its token from that pipe, it would compete with every running compiler's
//! codegen threads — and once a few big crates are compiling they hold every
//! token, so the next crate on the critical path waited for seconds (8 s of a
//! 16 s rebuild of Rune's own workspace) while its dependencies' codegen used
//! the cores.
//!
//! Here, whether a job starts is decided by the scheduler alone. A starting
//! job owes a token, and a background thread takes owed tokens from the pipe
//! — at once when one is free, otherwise as compilers hand theirs back (after
//! each codegen unit, well under a second). The
//! machine is at most briefly oversubscribed; the critical path never waits
//! behind another crate's codegen.

use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct Ledger {
    /// Running jobs that started without a token and are still owed one.
    owed: usize,
    /// Tokens the reclaimer took from the pipe on behalf of debtors.
    paid: usize,
    reclaiming: bool,
}

/// The token accounting of one build.
#[derive(Clone)]
pub struct JobTokens {
    client: jobserver::Client,
    ledger: Arc<(Mutex<Ledger>, Condvar)>,
}

/// One running job's claim on a core. Dropping it gives the token back.
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

    /// Claims a core for a job that is starting now (never blocks).
    pub fn start(&self) -> JobToken {
        // A pipe shared with child processes cannot be read without blocking
        // (making it non-blocking would change it for every compiler too),
        // so every job starts owing its token and a background thread takes
        // it from the pipe — at once when one is free.
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

    /// Takes tokens back from the pipe while jobs are owed one.
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
                // The debtor finished meanwhile.
                let _ = self.client.release_raw();
            }
        }
    }

    /// Tokens still owed (for tests and diagnostics).
    pub fn owed(&self) -> usize {
        self.ledger.0.lock().map(|l| l.owed).unwrap_or(0)
    }
}

impl Drop for JobToken {
    fn drop(&mut self) {
        let (lock, _) = &*self.tokens.ledger;
        let mut ledger = lock.lock().unwrap_or_else(|e| e.into_inner());
        if ledger.owed > 0 {
            // Never paid: nothing to give back.
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
        // Both free tokens are taken for them.
        assert_eq!(wait_owed(0), 0);
        // With none left (as when compilers' codegen threads hold them all),
        // a third job still starts at once, owing one.
        let c = tokens.start();
        assert_eq!(wait_owed(1), 1);
        // When a token comes back, the debt is paid from it.
        drop(a);
        assert_eq!(wait_owed(0), 0);
        drop(b);
        drop(c);
        // Every token is back in the pipe.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while client.available().unwrap() != 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(client.available().unwrap(), 2);
    }
}
