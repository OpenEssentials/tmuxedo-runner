//! "While you were away" in the TUI (PLAN 802).
//!
//! The web's digest (`GET /api/away`, token scope `read`), read with the
//! machines every 15 s: under the machines, a row per session with news
//! (needs you, finished, failed) saying what it did and the files the agent
//! touched (`+2 ~1 −0`, from the runner's `session.summary`). Enter on a row
//! goes to its pane; Enter on the heading opens the whole digest in the
//! browser, where Mark seen lives. A server without it (an older one), a
//! token without `read`, or no news shows nothing. The server masks every
//! text before it leaves.

use serde_json::Value;

/// Rows under the heading; the rest wait in the browser's digest.
pub const AWAY_ROWS_MAX: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub machine: String,
    pub pane: String,
    /// `needs_you`, `finished` or `failed`.
    pub group: String,
    pub title: String,
    pub turns: u64,
    /// (added, modified, removed), when the runner had anything to compare.
    pub files: Option<(u64, u64, u64)>,
    pub gone: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Away {
    pub cards: Vec<Card>,
}

const NEWS: [&str; 3] = ["needs_you", "finished", "failed"];

impl Away {
    /// From `{digest: {window, hasNews, groups: [{key, cards}]}}`; anything malformed is skipped.
    pub fn from_json(v: &Value) -> Away {
        let d = &v["digest"];
        if !d["window"].is_object() || d["hasNews"] != Value::Bool(true) {
            return Away::default();
        }
        let groups = d["groups"].as_array().cloned().unwrap_or_default();
        let cards = groups
            .iter()
            .filter(|g| g["key"].as_str().is_some_and(|k| NEWS.contains(&k)))
            .flat_map(|g| g["cards"].as_array().cloned().unwrap_or_default())
            .filter_map(|c| {
                let group = c["group"].as_str().filter(|k| NEWS.contains(k))?.to_string();
                let n = |f: &Value, k: &str| f[k].as_u64().unwrap_or(0);
                let files = c["files"]
                    .is_object()
                    .then(|| {
                        (
                            n(&c["files"], "added"),
                            n(&c["files"], "modified"),
                            n(&c["files"], "removed"),
                        )
                    })
                    .filter(|(a, m, r)| a + m + r > 0);
                Some(Card {
                    machine: c["machineId"].as_str()?.to_string(),
                    pane: c["paneId"].as_str()?.to_string(),
                    group,
                    title: clean(c["title"].as_str().unwrap_or("Session")),
                    turns: c["turns"].as_u64().unwrap_or(0),
                    files,
                    gone: c["gone"].as_bool().unwrap_or(false),
                })
            })
            .take(AWAY_ROWS_MAX)
            .collect();
        Away { cards }
    }
}

/// No control characters in a sidebar row (the server already masked it).
fn clean(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(60).collect();
    if t.trim().is_empty() {
        "Session".into()
    } else {
        t
    }
}

/// The row's glyph: `!` needs you, `✓` finished, `✗` failed.
pub fn glyph(group: &str) -> &'static str {
    match group {
        "needs_you" => "!",
        "finished" => "✓",
        _ => "✗",
    }
}

/// "build · +2 ~1 · 3 turns" (the files first: the sidebar is narrow; zeros dropped; "closed" when gone).
pub fn row_text(c: &Card) -> String {
    let mut parts = vec![c.title.clone()];
    if let Some((a, m, r)) = c.files {
        let marks: Vec<String> = [("+", a), ("~", m), ("−", r)]
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(s, n)| format!("{s}{n}"))
            .collect();
        parts.push(marks.join(" "));
    }
    if c.turns > 0 {
        parts.push(format!("{} turn{}", c.turns, if c.turns == 1 { "" } else { "s" }));
    }
    if c.gone {
        parts.push("closed".into());
    }
    parts.join(" · ")
}

/// The web's digest page.
pub fn open_url(web_url: &str) -> String {
    format!("{}/away", web_url.trim_end_matches('/'))
}

/// The digest, with the CLI's token. An answer other than 200 reads as none (older server, no `read`).
pub fn fetch(cfg: &super::super::CliConfig) -> Result<Away, String> {
    match super::super::api(cfg, "GET", "/api/away", None) {
        Ok((200, v)) => Ok(Away::from_json(&v)),
        Ok(_) => Ok(Away::default()),
        Err(e) => Err(format!("offline: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_news_only_and_skips_malformed_cards() {
        let d = json!({"digest": {"window": {"from":"a","until":"b"}, "hasNews": true, "groups": [
            {"key":"needs_you","cards":[{"machineId":"m1","paneId":"p1","group":"needs_you","title":"api\u{1b}[2J","turns":1}]},
            {"key":"working","cards":[{"machineId":"m1","paneId":"p2","group":"working","title":"w"}]},
            {"key":"finished","cards":[{"paneId":"p3","group":"finished"}, {"machineId":"m1","paneId":"p4","group":"finished","title":"","files":{"added":0,"modified":0,"removed":3},"gone":true}]}
        ]}});
        let a = Away::from_json(&d);
        assert_eq!(a.cards.len(), 2);
        assert_eq!(row_text(&a.cards[0]), "api[2J · 1 turn");
        assert_eq!(glyph(&a.cards[0].group), "!");
        assert_eq!(row_text(&a.cards[1]), "Session · −3 · closed");
        assert_eq!(Away::from_json(&json!({"error": "x"})), Away::default());
        assert_eq!(open_url("https://app.example.test/"), "https://app.example.test/away");
    }

    #[test]
    fn keeps_a_handful_of_rows() {
        let cards: Vec<Value> = (0..AWAY_ROWS_MAX + 2)
            .map(|i| json!({"machineId":"m1","paneId":format!("p{i}"),"group":"finished","title":"t"}))
            .collect();
        let d = json!({"digest": {"window": {}, "hasNews": true, "groups": [{"key":"finished","cards":cards}]}});
        assert_eq!(Away::from_json(&d).cards.len(), AWAY_ROWS_MAX);
    }
}
