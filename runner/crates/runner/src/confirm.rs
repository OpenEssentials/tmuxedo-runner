//! Confirm on this Mac (docs/design/mac-isolation.md §3): before a remote session
//! takes control of this machine, the person at the Mac is asked (Allow / Deny, a
//! timeout counts as Deny). Enforced HERE, in the runner, so it holds even if
//! tmuxedo's servers are compromised; the Mac app only shows the question.
//!
//! What counts as "taking control": the runner cannot see a browser's "Take
//! control" button (the gateway checks scopes, the runner sees frames), so the
//! first CONTROL action after a quiet period is the session start: typed input,
//! an answered prompt (decision), a mutating runner-API call, a login callback.
//! Watching (attach, reading methods) is never held; it is logged.
//!
//!   closed  --control action-->  pending (ask the app; hold what arrived)
//!   pending --allow-->           open (replay what was held, in order)
//!   pending --deny / timeout-->  closed, held work dropped, no new question
//!                                for the cool-off (a refused server cannot
//!                                spam the owner with notifications)
//!   open    --idle for `idle`--> closed (the next control action asks again)
//!
//! Held work is bounded (`hold_max` bytes); beyond it, input is dropped, never
//! queued without limit. Resizes are held (the latest per pane) but never ask.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    /// How long the Mac's question stays open; the app denies when it lapses.
    pub timeout: Duration,
    /// An approval lapses after this long without any control action.
    pub idle: Duration,
    /// After a Deny or a timeout, further control is dropped without asking for this long.
    pub cooloff: Duration,
    /// Bytes of held work (typed input) kept while the question is open.
    pub hold_max: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(30),
            idle: Duration::from_secs(15 * 60),
            cooloff: Duration::from_secs(30),
            hold_max: 64 * 1024,
        }
    }
}

/// What the gate says about one control action.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit<T> {
    /// Approved: do it now.
    Pass(T),
    /// A question is open: kept, done if allowed.
    Held,
    /// Start a question now (the item is held).
    Ask,
    /// Refused: the reason, for the log (logged once per question, not per keystroke).
    Dropped(Option<&'static str>),
}

/// What the gate says about a call that cannot wait (a runner-API request).
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Pass,
    /// Start a question now; refuse this call ("asked on the Mac, try again").
    Ask,
    /// A question is already open.
    Pending,
    /// Recently denied.
    Denied,
}

#[derive(Debug)]
pub struct Gate<T> {
    pub policy: Policy,
    /// Approved, with the last control action at this instant.
    open_since_activity: Option<Instant>,
    /// A question is open since.
    pending: Option<Instant>,
    held: Vec<(T, usize)>,
    held_bytes: usize,
    denied_until: Option<Instant>,
    dropped_noted: bool,
    /// Numbers the questions, so a late answer never settles a newer one.
    question: u64,
}

impl<T> Gate<T> {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            open_since_activity: None,
            pending: None,
            held: vec![],
            held_bytes: 0,
            denied_until: None,
            dropped_noted: false,
            question: 0,
        }
    }

    /// The number of the current (or last) question.
    pub fn question(&self) -> u64 {
        self.question
    }

    fn start_question(&mut self, now: Instant) {
        self.pending = Some(now);
        self.dropped_noted = false;
        self.question += 1;
    }

    pub fn enabled(&self) -> bool {
        self.policy.enabled
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// Approved and not idle.
    pub fn is_open(&self, now: Instant) -> bool {
        !self.policy.enabled
            || self
                .open_since_activity
                .is_some_and(|t| now.saturating_duration_since(t) < self.policy.idle)
    }

    fn denied(&self, now: Instant) -> bool {
        self.denied_until.is_some_and(|t| now < t)
    }

    fn drop_reason(&mut self, why: &'static str) -> Admit<T> {
        if self.dropped_noted {
            Admit::Dropped(None)
        } else {
            self.dropped_noted = true;
            Admit::Dropped(Some(why))
        }
    }

    /// A control action that can wait for the answer (input, a decision). `asks`: false for a
    /// resize, which is held but never starts a question by itself.
    pub fn admit(&mut self, now: Instant, item: T, size: usize, asks: bool) -> Admit<T> {
        if self.is_open(now) {
            if self.policy.enabled {
                self.open_since_activity = Some(now);
            }
            return Admit::Pass(item);
        }
        self.open_since_activity = None;
        if self.pending.is_some() {
            return self.hold(item, size);
        }
        if self.denied(now) {
            return self.drop_reason("refused: this Mac denied remote control a moment ago");
        }
        if !asks {
            // Kept for an approval that may come; never asks.
            return match self.hold(item, size) {
                Admit::Held => Admit::Held,
                other => other,
            };
        }
        self.start_question(now);
        match self.hold(item, size) {
            Admit::Held => Admit::Ask,
            other => other,
        }
    }

    fn hold(&mut self, item: T, size: usize) -> Admit<T> {
        if self.held_bytes + size > self.policy.hold_max {
            return self.drop_reason("dropped: too much input while waiting for this Mac's answer");
        }
        self.held_bytes += size;
        self.held.push((item, size));
        Admit::Held
    }

    /// Keep only the newest of the held items `same` matches (a resize replaces an older one).
    pub fn replace_held(&mut self, same: impl Fn(&T) -> bool) {
        if let Some(last) = self.held.iter().rposition(|(t, _)| same(t)) {
            let mut i = 0;
            self.held.retain(|(t, _)| {
                let keep = i == last || !same(t);
                i += 1;
                keep
            });
            self.held_bytes = self.held.iter().map(|(_, s)| s).sum();
        }
    }

    /// A call that cannot be held (it needs an answer now).
    pub fn check(&mut self, now: Instant) -> Check {
        if self.is_open(now) {
            if self.policy.enabled {
                self.open_since_activity = Some(now);
            }
            return Check::Pass;
        }
        if self.pending.is_some() {
            return Check::Pending;
        }
        if self.denied(now) {
            return Check::Denied;
        }
        self.start_question(now);
        Check::Ask
    }

    /// The Mac answered. Allowed: the held work, in arrival order, to do now.
    pub fn resolve(&mut self, now: Instant, allowed: bool) -> Vec<T> {
        self.pending = None;
        self.held_bytes = 0;
        let held = std::mem::take(&mut self.held);
        if allowed {
            self.open_since_activity = Some(now);
            self.denied_until = None;
            held.into_iter().map(|(t, _)| t).collect()
        } else {
            self.open_since_activity = None;
            self.denied_until = Some(now + self.policy.cooloff);
            vec![]
        }
    }

    /// A safety net: a question open far beyond its timeout (the asker vanished) counts as denied.
    pub fn expire(&mut self, now: Instant) -> bool {
        let limit = self.policy.timeout + Duration::from_secs(15);
        if self.pending.is_some_and(|t| now.saturating_duration_since(t) > limit) {
            self.resolve(now, false);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> Policy {
        Policy {
            enabled: true,
            timeout: Duration::from_secs(30),
            idle: Duration::from_secs(600),
            cooloff: Duration::from_secs(30),
            hold_max: 10,
        }
    }

    #[test]
    fn off_passes_everything() {
        let mut g: Gate<u8> = Gate::new(Policy::default());
        let t = Instant::now();
        assert_eq!(g.admit(t, 1, 1, true), Admit::Pass(1));
        assert_eq!(g.check(t), Check::Pass);
        assert!(g.is_open(t));
    }

    #[test]
    fn the_first_control_action_asks_and_is_held_until_allowed() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        assert_eq!(g.admit(t, "a", 1, true), Admit::Ask);
        assert!(g.is_pending());
        assert_eq!(
            g.admit(t, "b", 1, true),
            Admit::Held,
            "one question, not one per keystroke"
        );
        assert_eq!(g.check(t), Check::Pending);
        assert_eq!(g.resolve(t, true), vec!["a", "b"], "replayed in order");
        assert_eq!(g.admit(t, "c", 1, true), Admit::Pass("c"));
        assert_eq!(g.check(t), Check::Pass);
    }

    #[test]
    fn deny_drops_the_held_work_and_cools_off() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        assert_eq!(g.admit(t, 1, 1, true), Admit::Ask);
        assert_eq!(g.resolve(t, false), Vec::<i32>::new());
        assert!(matches!(g.admit(t, 2, 1, true), Admit::Dropped(Some(_))));
        assert_eq!(g.admit(t, 3, 1, true), Admit::Dropped(None), "noted once");
        assert_eq!(g.check(t + Duration::from_secs(5)), Check::Denied);
        // After the cool-off, a new question.
        assert_eq!(g.admit(t + Duration::from_secs(31), 4, 1, true), Admit::Ask);
    }

    #[test]
    fn an_approval_lapses_after_idle_but_activity_keeps_it() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        g.admit(t, 1, 1, true);
        g.resolve(t, true);
        let t2 = t + Duration::from_secs(500);
        assert_eq!(g.admit(t2, 2, 1, true), Admit::Pass(2));
        let t3 = t2 + Duration::from_secs(500);
        assert_eq!(g.admit(t3, 3, 1, true), Admit::Pass(3), "activity extended it");
        assert_eq!(g.admit(t3 + Duration::from_secs(601), 4, 1, true), Admit::Ask);
    }

    #[test]
    fn held_work_is_bounded() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        assert_eq!(g.admit(t, 1, 8, true), Admit::Ask);
        assert!(matches!(g.admit(t, 2, 8, true), Admit::Dropped(Some(_))));
        assert_eq!(g.admit(t, 3, 2, true), Admit::Held);
        assert_eq!(g.resolve(t, true), vec![1, 3]);
    }

    #[test]
    fn resizes_are_held_but_never_ask_and_only_the_newest_is_kept() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        assert_eq!(g.admit(t, ("resize", 1), 0, false), Admit::Held);
        assert!(!g.is_pending(), "a resize alone asks nothing");
        assert_eq!(g.admit(t, ("resize", 2), 0, false), Admit::Held);
        g.replace_held(|x| x.0 == "resize");
        assert_eq!(g.held_len(), 1);
        assert_eq!(g.admit(t, ("input", 3), 1, true), Admit::Ask);
        assert_eq!(g.resolve(t, true), vec![("resize", 2), ("input", 3)]);
    }

    #[test]
    fn a_call_that_cannot_wait_asks_and_is_refused_until_allowed() {
        let mut g: Gate<()> = Gate::new(on());
        let t = Instant::now();
        assert_eq!(g.question(), 0);
        assert_eq!(g.check(t), Check::Ask);
        assert_eq!(g.question(), 1, "each question is numbered");
        assert_eq!(g.check(t), Check::Pending);
        g.resolve(t, true);
        assert_eq!(g.check(t), Check::Pass);
    }

    #[test]
    fn a_question_nobody_answers_expires_as_denied() {
        let mut g = Gate::new(on());
        let t = Instant::now();
        g.admit(t, 1, 1, true);
        assert!(!g.expire(t + Duration::from_secs(40)));
        assert!(g.expire(t + Duration::from_secs(46)));
        assert!(!g.is_pending());
        assert_eq!(g.check(t + Duration::from_secs(47)), Check::Denied);
    }
}
