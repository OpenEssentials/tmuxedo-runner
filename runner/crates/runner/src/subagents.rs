//! Helpers: the sub-agents a pane's agent runs inside its own process
//! (docs/design/multi-agent-workflows.md §4.1 "Observe", docs/protocol.md
//! "Helpers"). Plugins translate their CLI's hooks into `subagent.started` /
//! `subagent.completed` (manifest `hooks.map`); this keeps the list one
//! agent shows as `Agent.children`.
//!
//! Bounds: at most [`MAX_AGENT_CHILDREN`] entries; a finished helper is kept
//! for [`DONE_RETENTION`]; one that never reports its end is dropped after
//! [`STALE_AFTER`]. The list ends with the agent (session end, process gone).

use std::time::{Duration, Instant};
use tmuxedo_protocol::subagents::{
    cut, AgentChild, AgentChildStatus, AGENT_CHILD_ID_MAX, AGENT_CHILD_NAME_MAX, MAX_AGENT_CHILDREN,
};

/// How long a finished helper stays in the list.
pub const DONE_RETENTION: Duration = Duration::from_secs(10 * 60);
/// A helper with no end after this long is dropped (its stop was lost).
pub const STALE_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// What a `subagent.started` carries; every field is optional on the wire.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Started {
    pub id: Option<String>,
    pub name: String,
    pub parent_id: Option<String>,
    pub model: Option<String>,
    /// The role it was built from (crate::role_launch::role_of_helper), set by the daemon.
    pub role_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    child: AgentChild,
    /// When the current status began, on this process's clock.
    at: Instant,
}

impl Entry {
    fn working(&self) -> bool {
        self.child.status == AgentChildStatus::Working
    }
}

#[derive(Debug, Clone, Default)]
pub struct Children {
    entries: Vec<Entry>,
    /// Counter for ids of helpers whose CLI gives none.
    next: u64,
}

fn clean(s: Option<String>, max: usize) -> Option<String> {
    s.map(|s| cut(s.trim(), max)).filter(|s| !s.is_empty())
}

impl Children {
    /// The list as the protocol carries it, oldest first.
    pub fn list(&self) -> Vec<AgentChild> {
        self.entries.iter().map(|e| e.child.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn working(&self) -> usize {
        self.entries.iter().filter(|e| e.working()).count()
    }

    fn working_index(&self, id: &str) -> Option<usize> {
        let hit = |e: &Entry| e.child.id == id && e.working();
        self.entries.iter().position(hit)
    }

    /// A helper started. None when it is already known as working (a
    /// repeated hook): nothing changed and nothing is announced.
    pub fn start(&mut self, s: Started, now: Instant, iso: &str) -> Option<AgentChild> {
        let id = match clean(s.id, AGENT_CHILD_ID_MAX) {
            Some(id) => {
                if self.working_index(&id).is_some() {
                    return None;
                }
                // The same id again after it finished: the new run replaces the old row.
                self.entries.retain(|e| e.child.id != id);
                id
            }
            None => loop {
                self.next += 1;
                let id = format!("helper-{}", self.next);
                if self.entries.iter().all(|e| e.child.id != id) {
                    break id;
                }
            },
        };
        let child = AgentChild {
            id,
            name: cut(s.name.trim(), AGENT_CHILD_NAME_MAX),
            status: AgentChildStatus::Working,
            since: iso.to_string(),
            started_at: Some(iso.to_string()),
            parent_id: clean(s.parent_id, AGENT_CHILD_ID_MAX),
            model: clean(s.model, AGENT_CHILD_NAME_MAX),
            role_id: s.role_id.filter(|r| tmuxedo_protocol::roles::role_id_ok(r)),
            needs: None,
        };
        self.entries.push(Entry {
            child: child.clone(),
            at: now,
        });
        self.bound();
        Some(child)
    }

    /// A helper finished. With an id: that helper. Without one (the CLI's
    /// stop hook names none): the newest working helper of that name, else
    /// the newest working one (helpers nest, so the last started ends
    /// first). None when no working helper matches.
    pub fn complete(&mut self, id: Option<&str>, name: Option<&str>, now: Instant, iso: &str) -> Option<AgentChild> {
        let i = match clean(id.map(str::to_owned), AGENT_CHILD_ID_MAX) {
            Some(id) => self.working_index(&id)?,
            None => {
                let name = name.map(str::trim).filter(|n| !n.is_empty());
                let named = |e: &Entry| name.is_some_and(|n| e.child.name == n);
                let all = &self.entries;
                let by_name = all.iter().rposition(|e| e.working() && named(e));
                by_name.or_else(|| all.iter().rposition(Entry::working))?
            }
        };
        let e = &mut self.entries[i];
        e.child.status = AgentChildStatus::Done;
        e.child.since = iso.to_string();
        e.at = now;
        Some(e.child.clone())
    }

    /// Keep at most [`MAX_AGENT_CHILDREN`]: the oldest finished helper goes
    /// first, then the oldest working one.
    fn bound(&mut self) {
        while self.entries.len() > MAX_AGENT_CHILDREN {
            let done = |e: &Entry| !e.working();
            let i = self.entries.iter().position(done).unwrap_or(0);
            self.entries.remove(i);
        }
    }

    /// Drop finished helpers past their retention and working ones that
    /// never ended. True when the list changed.
    pub fn prune(&mut self, now: Instant) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| {
            let age = now.saturating_duration_since(e.at);
            match e.child.status {
                AgentChildStatus::Done => age < DONE_RETENTION,
                AgentChildStatus::Working => age < STALE_AFTER,
            }
        });
        self.entries.len() != before
    }

    /// The agent is gone or its session ended. True when there was a list.
    pub fn clear(&mut self) -> bool {
        let had = !self.entries.is_empty();
        self.entries.clear();
        had
    }

    /// A list saved before a runner restart: the helpers still run in the
    /// agent's process, so they come back; their clocks start again.
    pub fn restore(saved: &[AgentChild], now: Instant) -> Children {
        let mut c = Children::default();
        for child in saved.iter().take(MAX_AGENT_CHILDREN) {
            c.entries.push(Entry {
                child: child.clone(),
                at: now,
            });
        }
        c.next = saved.len() as u64;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "2026-10-12T10:00:00.000Z";
    const T2: &str = "2026-10-12T10:05:00.000Z";

    fn started(id: Option<&str>, name: &str) -> Started {
        Started {
            id: id.map(str::to_owned),
            name: name.into(),
            ..Started::default()
        }
    }

    fn ids(c: &Children) -> Vec<String> {
        c.list().into_iter().map(|c| c.id).collect()
    }

    #[test]
    fn start_then_complete_by_id() {
        let now = Instant::now();
        let mut c = Children::default();
        let a = c.start(started(Some("a1"), "Explore"), now, T).unwrap();
        assert_eq!((a.id.as_str(), a.name.as_str()), ("a1", "Explore"));
        assert_eq!(a.status, AgentChildStatus::Working);
        assert_eq!(a.started_at.as_deref(), Some(T));
        assert_eq!(c.working(), 1);
        // The same start again says nothing new.
        assert!(c.start(started(Some("a1"), "Explore"), now, T).is_none());
        let d = c.complete(Some("a1"), None, now, T2).unwrap();
        assert_eq!(d.status, AgentChildStatus::Done);
        assert_eq!((d.since.as_str(), d.started_at.as_deref()), (T2, Some(T)));
        assert_eq!(c.working(), 0);
        // A second stop, or a stop for a helper nobody saw start, changes nothing.
        assert!(c.complete(Some("a1"), None, now, T2).is_none());
        assert!(c.complete(Some("zz"), None, now, T2).is_none());
        assert_eq!(c.list().len(), 1);
    }

    #[test]
    fn nested_helpers_keep_their_parent() {
        let now = Instant::now();
        let mut c = Children::default();
        c.start(started(Some("a1"), "builder"), now, T);
        let inner = Started {
            id: Some("a2".into()),
            name: "tester".into(),
            parent_id: Some("a1".into()),
            role_id: Some("tester".into()),
            model: Some("sonnet".into()),
        };
        let b = c.start(inner, now, T).unwrap();
        assert_eq!(b.parent_id.as_deref(), Some("a1"));
        assert_eq!(b.model.as_deref(), Some("sonnet"));
        assert_eq!(b.role_id.as_deref(), Some("tester"));
        // The parent ending leaves the inner helper as it is.
        c.complete(Some("a1"), None, now, T2);
        assert_eq!(c.working(), 1);
        assert_eq!(ids(&c), ["a1", "a2"]);
    }

    #[test]
    fn helpers_without_ids_pair_by_name_then_newest_first() {
        let now = Instant::now();
        let mut c = Children::default();
        let a = c.start(started(None, "Explore"), now, T).unwrap();
        let b = c.start(started(None, "tester"), now, T).unwrap();
        let e = c.start(started(None, "Explore"), now, T).unwrap();
        assert_eq!(
            [a.id.as_str(), b.id.as_str(), e.id.as_str()],
            ["helper-1", "helper-2", "helper-3"]
        );
        // By name: the newest working "Explore".
        let x = c.complete(None, Some("Explore"), now, T2).unwrap();
        assert_eq!(x.id, "helper-3");
        // No name, or a name nobody has: the newest working one.
        let x = c.complete(None, None, now, T2).unwrap();
        assert_eq!(x.id, "helper-2");
        let x = c.complete(None, Some("nobody"), now, T2).unwrap();
        assert_eq!(x.id, "helper-1");
        assert!(c.complete(None, None, now, T2).is_none());
    }

    #[test]
    fn the_list_is_bounded_finished_first() {
        let now = Instant::now();
        let mut c = Children::default();
        for i in 0..MAX_AGENT_CHILDREN {
            c.start(started(Some(&format!("k{i}")), "x"), now, T);
        }
        c.complete(Some("k3"), None, now, T2);
        // One over: the finished helper makes room.
        c.start(started(Some("new1"), "x"), now, T);
        assert_eq!(c.list().len(), MAX_AGENT_CHILDREN);
        assert!(!ids(&c).contains(&"k3".to_string()));
        // All working: the oldest goes.
        c.start(started(Some("new2"), "x"), now, T);
        assert_eq!(c.list().len(), MAX_AGENT_CHILDREN);
        assert!(!ids(&c).contains(&"k0".to_string()));
        assert_eq!(ids(&c).last().map(String::as_str), Some("new2"));
    }

    #[test]
    fn long_and_blank_fields_are_cut() {
        let now = Instant::now();
        let mut c = Children::default();
        let s = Started {
            id: Some("i".repeat(500)),
            name: format!("  {}  ", "n".repeat(500)),
            parent_id: Some("   ".into()),
            model: Some(String::new()),
            role_id: Some("Not A Role".into()),
        };
        let a = c.start(s, now, T).unwrap();
        assert_eq!(a.id.len(), AGENT_CHILD_ID_MAX);
        assert_eq!(a.name.len(), AGENT_CHILD_NAME_MAX);
        assert_eq!((a.parent_id, a.model, a.role_id), (None, None, None));
        // A blank id is no id.
        let b = c.start(started(Some("  "), "x"), now, T).unwrap();
        assert_eq!(b.id, "helper-1");
    }

    #[test]
    fn finished_helpers_expire_and_one_that_never_stops_is_dropped() {
        let t0 = Instant::now();
        let mut c = Children::default();
        c.start(started(Some("quick"), "x"), t0, T);
        c.start(started(Some("stuck"), "x"), t0, T);
        c.complete(Some("quick"), None, t0, T2);
        assert!(!c.prune(t0 + DONE_RETENTION - Duration::from_secs(1)));
        assert!(c.prune(t0 + DONE_RETENTION));
        assert_eq!(ids(&c), ["stuck"]);
        assert!(!c.prune(t0 + STALE_AFTER - Duration::from_secs(1)));
        assert!(c.prune(t0 + STALE_AFTER));
        assert!(c.is_empty());
        assert!(!c.clear());
    }

    #[test]
    fn a_saved_list_comes_back_after_a_restart() {
        let t0 = Instant::now();
        let mut c = Children::default();
        c.start(started(None, "Explore"), t0, T);
        c.start(started(Some("a2"), "tester"), t0, T);
        c.complete(Some("a2"), None, t0, T2);
        let saved = c.list();
        let later = t0 + Duration::from_secs(3600);
        let mut r = Children::restore(&saved, later);
        assert_eq!(r.list(), saved);
        assert_eq!(r.working(), 1);
        // The stop that arrives after the restart still finds its helper.
        let x = r.complete(None, Some("Explore"), later, T2).unwrap();
        assert_eq!(x.id, "helper-1");
        // A new id-less helper does not reuse a restored id.
        let x = r.start(started(None, "x"), later, T).unwrap();
        assert_eq!(x.id, "helper-3");
        assert!(r.clear());
    }

    #[test]
    fn the_saved_list_rides_in_the_pane_record() {
        use crate::topology::AgentMeta;
        let mut m = AgentMeta::default();
        let empty = serde_json::to_value(&m).unwrap();
        assert!(empty.get("helpers").is_none());
        let mut c = Children::default();
        c.start(started(Some("a1"), "Explore"), Instant::now(), T);
        m.helpers = c.list();
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["helpers"][0]["id"], "a1");
        let back: AgentMeta = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
        // A state file from before this has no list.
        let old: AgentMeta = serde_json::from_value(empty).unwrap();
        assert!(old.helpers.is_empty());
    }
}
