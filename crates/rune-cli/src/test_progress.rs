//! The live line `rune test` shows: binaries done, tests passed so far, and what is running now.

use indicatif::ProgressBar;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cmd_test::Counts;
use crate::ui;

struct Running {
    label: String,
    started: Instant,
    total: u64,
    done: u64,
    failed: u64,
    slow: Vec<String>,
}

#[derive(Default)]
struct State {
    running: BTreeMap<usize, Running>,
    passed: u64,
    failed: u64,
    done: u64,
}

struct Shown {
    bar: ProgressBar,
    stop_ticker: Option<crossbeam_channel::Sender<()>>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

pub struct TestProgress {
    enabled: bool,
    slots: u64,
    shown: Mutex<Option<Shown>>,
    state: Arc<Mutex<State>>,
    stop: AtomicBool,
}

impl TestProgress {
    pub fn new(slots: usize, enabled: bool) -> Self {
        Self {
            enabled,
            slots: slots as u64,
            shown: Mutex::new(None),
            state: Arc::default(),
            stop: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn shown(&self) -> std::sync::MutexGuard<'_, Option<Shown>> {
        self.shown.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn show(&self) {
        if !self.enabled || self.stop.load(Ordering::Relaxed) {
            return;
        }
        let mut shown = self.shown();
        if shown.is_some() {
            return;
        }
        let bar = ui::multi_progress().add(ProgressBar::new(self.slots));
        bar.set_style(rune_scheduler::progress::bar_style(
            rune_scheduler::progress::bar_width(),
        ));
        bar.set_prefix("testing");
        draw(&bar, &self.state);
        let (mut stop_ticker, mut ticker) = (None, None);
        if !bar.is_hidden() {
            bar.enable_steady_tick(Duration::from_millis(80));
            let (tx, rx) = crossbeam_channel::bounded::<()>(0);
            let (bar, state) = (bar.clone(), self.state.clone());
            ticker = Some(std::thread::spawn(move || {
                while let Err(crossbeam_channel::RecvTimeoutError::Timeout) =
                    rx.recv_timeout(Duration::from_millis(250))
                {
                    draw(&bar, &state);
                }
            }));
            stop_ticker = Some(tx);
        }
        *shown = Some(Shown {
            bar,
            stop_ticker,
            ticker,
        });
    }

    fn redraw(&self) {
        if let Some(s) = self.shown().as_ref() {
            draw(&s.bar, &self.state);
        }
    }

    pub fn begin(&self, id: usize, label: &str) {
        self.lock().running.insert(
            id,
            Running {
                label: label.to_string(),
                started: Instant::now(),
                total: 0,
                done: 0,
                failed: 0,
                slow: Vec::new(),
            },
        );
        self.show();
        self.redraw();
    }

    pub fn line(&self, id: usize, line: &str) {
        let line = line.trim_end();
        let mut changed = false;
        {
            let mut st = self.lock();
            let Some(r) = st.running.get_mut(&id) else {
                return;
            };
            if let Some(n) = line
                .strip_prefix("running ")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
            {
                r.total += n;
                changed = true;
            } else if let Some((name, _)) = line
                .strip_prefix("test ")
                .filter(|rest| rest.ends_with(" seconds"))
                .and_then(|rest| rest.rsplit_once(" has been running for over "))
            {
                r.slow.push(name.to_string());
                changed = true;
            } else if let Some(rest) = line.strip_prefix("test ")
                && let Some((name, _)) = rest.split_once(" ... ")
            {
                r.done += 1;
                if line.ends_with("FAILED") {
                    r.failed += 1;
                }
                r.slow.retain(|s| s != name);
                changed = true;
            }
        }
        if changed {
            self.redraw();
        }
    }

    pub fn end(&self, id: usize) {
        self.lock().running.remove(&id);
        self.redraw();
    }

    pub fn slot_done(&self, counts: Counts) {
        {
            let mut st = self.lock();
            st.passed += counts.passed;
            st.failed += counts.failed;
            st.done += 1;
        }
        self.redraw();
    }

    pub fn finish(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(Shown {
            bar,
            stop_ticker,
            ticker,
        }) = self.shown().take()
        {
            drop(stop_ticker);
            if let Some(t) = ticker {
                let _ = t.join();
            }
            bar.finish_and_clear();
            ui::multi_progress().remove(&bar);
        }
    }
}

impl Drop for TestProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

fn draw(bar: &ProgressBar, state: &Mutex<State>) {
    let done = state.lock().unwrap_or_else(|e| e.into_inner()).done;
    bar.set_position(done);
    bar.set_message(message(bar, state));
}

fn message(bar: &ProgressBar, state: &Mutex<State>) -> String {
    let st = state.lock().unwrap_or_else(|e| e.into_inner());
    let live_passed: u64 = st.running.values().map(|r| r.done - r.failed).sum();
    let live_failed: u64 = st.running.values().map(|r| r.failed).sum();
    let passed = st.passed + live_passed;
    let failed = st.failed + live_failed;
    let mut head = format!("{passed} passed");
    if failed > 0 {
        head.push_str(&format!(", {failed} failed"));
    }
    if st.running.is_empty() {
        return format!("· {head} · waiting for the build");
    }
    let mut running: Vec<&Running> = st.running.values().collect();
    running.sort_by_key(|r| r.started);
    let names: Vec<String> = running
        .iter()
        .map(|r| {
            let mut s = r.label.clone();
            if r.total > 0 {
                s.push_str(&format!(" {}/{}", r.done, r.total));
            }
            let secs = r.started.elapsed().as_secs();
            if secs >= 2 {
                s.push_str(&format!(" {secs}s"));
            }
            if let Some(first) = r.slow.first() {
                s.push_str(&format!(" (slow: {first}"));
                if r.slow.len() > 1 {
                    s.push_str(&format!(" +{}", r.slow.len() - 1));
                }
                s.push(')');
            }
            s
        })
        .collect();
    let used = 2
        + bar.prefix().chars().count()
        + 1
        + rune_scheduler::progress::bar_width()
        + 1
        + format!("{}/{}", bar.position(), bar.length().unwrap_or(0)).len()
        + 3
        + head.chars().count()
        + 3;
    let width = console::Term::stderr()
        .size_checked()
        .map(|(_, w)| w as usize)
        .unwrap_or(80);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let list = rune_scheduler::progress::fit_list(&refs, width.saturating_sub(used + 1));
    if list.is_empty() {
        head
    } else {
        format!("· {head} · {list}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libtest_lines_move_the_counts() {
        let p = TestProgress::new(2, false);
        p.begin(1, "demo (test it)");
        for l in [
            "running 3 tests\n",
            "test a ... ok\n",
            "test b ... FAILED\n",
            "test c ... ignored, slow\n",
            "some output of a test\n",
            "test d has been running for over 60 seconds\n",
        ] {
            p.line(1, l);
        }
        {
            let st = p.lock();
            let r = &st.running[&1];
            assert_eq!((r.total, r.done, r.failed), (3, 3, 1));
            assert_eq!(r.slow, ["d"]);
        }
        p.line(1, "test d ... ok\n");
        assert!(
            p.lock().running[&1].slow.is_empty(),
            "a slow test that finished"
        );
        p.end(1);
        p.slot_done(Counts {
            passed: 1,
            failed: 1,
            ignored: 1,
            filtered: 0,
        });
        let st = p.lock();
        assert!(st.running.is_empty());
        assert_eq!((st.passed, st.failed), (1, 1));
    }
}
