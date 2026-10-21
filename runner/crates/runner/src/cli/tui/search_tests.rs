//! `prefix /` search in the TUI (super::search): keys to the same requests
//! the web makes, answers grouped and drawn at odd sizes, refusals and
//! offline machines in the web's words, hostile answers made harmless, and
//! Enter opening a pane (at the line), a chat or a project.

use super::app::{App, Mode, Out};
use super::search::{self, Answer, Jump, Kind, Request, Target};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tmuxedo_protocol::{DataFrame, DataKind};

const SIZES: [(u16, u16); 4] = [(140, 40), (80, 24), (44, 12), (24, 8)];

fn topology() -> Value {
    let pane = |id: &str| json!({"id":id,"tabId":"w1:t1","cwd":"/work","command":"sh","title":"sh","cols":60,"rows":10,"agent":null});
    json!({
        "t": "topology", "rev": 3,
        "workspaces": [{"id":"w1","name":"default","cwd":"/work","position":0},
                       {"id":"w2","name":"billing","cwd":"/srv","position":1}],
        "tabs": [{"id":"w1:t1","workspaceId":"w1","name":"main","position":0,
                  "layout":{"dir":"h","ratio":0.5,"a":{"pane":"w1:p1"},"b":{"pane":"w1:p2"}},"focusedPane":"w1:p1"}],
        "panes": [pane("w1:p1"), pane("w1:p2")],
    })
}

/// Two machines: m1 "box" (online, attached) and m2 "lab" (offline).
fn app(size: (u16, u16)) -> App {
    let mut app = App::new(true, size);
    app.on_machines(&[
        json!({"id":"m1","name":"box","online":true}),
        json!({"id":"m2","name":"lab","online":false}),
    ]);
    app.select_initial(None);
    app.on_text(
        "m1",
        &json!({"t":"welcome","sub":"u1","mid":"m1","scope":"control","machine":{"online":true}}),
    );
    app.on_text("m1", &topology());
    app.out.clear();
    app.web_url = "https://tmuxedo.example".into();
    app
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn prefixed(app: &mut App, c: KeyCode) {
    app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    app.on_key(key(c));
}

fn typed(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
}

/// Past the debounce: the requests it made.
fn fire(app: &mut App) -> Vec<Request> {
    app.search_tick(Instant::now() + Duration::from_secs(1));
    app.search_out.drain(..).collect()
}

fn screen(app: &App) -> Vec<String> {
    let (w, h) = app.size;
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| view::draw(app, f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn gen(app: &App) -> u64 {
    app.search.as_ref().unwrap().gen
}

fn server_answer() -> Value {
    json!({"query":"deploy","truncated":false,"hits":[
        {"id":"s1","kind":"session","title":"api tests","snippet":"the deploy failed","match":[4,10],
         "machineId":"m1","machineName":"box","at":"2026-10-18T14:02:11.000Z","href":"/machines/m1?pane=w1%3Ap2","score":3},
        {"id":"w2","kind":"project","title":"billing","snippet":"billing deploy","match":[8,14],
         "machineId":"m1","machineName":"box","at":null,"href":"/machines/m1?ws=w2","score":2},
        {"id":"e1","kind":"event","title":"Needs you","snippet":"deploy waiting","match":[0,6],
         "machineId":"m1","machineName":"box","at":"2026-10-18T15:00:00Z","href":"/machines/m1?pane=w1%3Ap1","score":1}
    ]})
}

fn machine_answer() -> Value {
    json!({"state":"done","partial":false,"more":false,"hits":[
        {"id":"t1","kind":"terminal","project":"default","title":"sh","agent":null,"session":null,
         "snippet":"…make deploy FAILED at step 3…","match":[6,12],"at":"2026-10-18T14:00:00Z",
         "href":"/machines/m1?pane=w1%3Ap1&line=42"},
        {"id":"c1","kind":"chat","project":"default","title":"fix deploy","agent":"claude","session":"sess-1",
         "snippet":"why did deploy fail","match":[8,14],"at":"2026-10-18T13:00:00Z",
         "href":"/machines/m1?chat=sess-1&find=deploy"}
    ]})
}

/// Open, type "deploy", let the requests go and the answers come back.
fn answered(size: (u16, u16)) -> App {
    let mut a = app(size);
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "deploy");
    fire(&mut a);
    let g = gen(&a);
    a.on_search(g, Target::Server, Ok((200, server_answer())));
    a.on_search(g, Target::Machine("m1".into()), Ok((200, machine_answer())));
    a
}

#[test]
fn prefix_slash_opens_and_typing_asks_the_server_and_online_machines() {
    let mut a = app((140, 40));
    prefixed(&mut a, KeyCode::Char('/'));
    assert_eq!(a.mode, Mode::Search);
    typed(&mut a, "dep");
    // Nothing is asked before the pause.
    a.search_tick(Instant::now());
    assert!(a.search_out.is_empty());
    typed(&mut a, "loy now");
    let r = fire(&mut a);
    assert_eq!(
        r,
        vec![
            Request {
                gen: 1,
                target: Target::Server,
                path: "/api/search?q=deploy%20now".into()
            },
            Request {
                gen: 1,
                target: Target::Machine("m1".into()),
                path: "/api/search/machines/m1?q=deploy%20now".into()
            },
        ],
        "the offline machine is not asked"
    );
    let v = a.search.as_ref().unwrap();
    assert_eq!(v.machines[1].2, Answer::Offline);
    assert!(screen(&a)
        .iter()
        .any(|l| l.contains("lab is offline; will search when it's back")));
    // Typing nothing else: nothing more is asked.
    assert!(fire(&mut a).is_empty());
}

#[test]
fn tab_changes_the_kind_with_the_webs_rules() {
    let mut a = app((140, 40));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "x");
    fire(&mut a);
    let mut seen = vec![];
    for _ in 0..7 {
        a.on_key(key(KeyCode::Tab));
        let k = a.search.as_ref().unwrap().kind;
        let paths: Vec<String> = fire(&mut a).into_iter().map(|r| r.path).collect();
        seen.push((k, paths));
    }
    assert_eq!(
        seen[0],
        (Kind::Sessions, vec!["/api/search?q=x&kind=session".to_string()])
    );
    assert_eq!(
        seen[3],
        (Kind::Activity, vec!["/api/search?q=x&kind=event".to_string()])
    );
    assert_eq!(
        seen[4],
        (
            Kind::Terminal,
            vec!["/api/search/machines/m1?q=x&kind=terminal".to_string()]
        )
    );
    assert_eq!(
        seen[5],
        (Kind::Chats, vec!["/api/search/machines/m1?q=x&kind=chat".to_string()])
    );
    assert_eq!(seen[6].0, Kind::All);
    assert_eq!(seen[6].1.len(), 2);
    a.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(a.search.as_ref().unwrap().kind, Kind::Chats);
}

#[test]
fn the_query_is_normalised_like_the_webs() {
    assert_eq!(search::normalise("  a\t\tb \u{7}c  "), Some("a b c".into()));
    assert_eq!(search::normalise(" \n "), None);
    assert_eq!(search::normalise(&"x".repeat(500)).unwrap().len(), 200);
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    a.on_event(Event::Paste("line one\nline two".into()));
    let r = fire(&mut a);
    assert_eq!(r[0].path, "/api/search?q=line%20one%20line%20two");
    // Typing stops at 200 characters.
    typed(&mut a, &"y".repeat(300));
    assert_eq!(a.search.as_ref().unwrap().query.chars().count(), 200);
    // Ctrl-U clears: nothing is asked, the hint is back.
    a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(fire(&mut a).is_empty());
    assert!(screen(&a).iter().any(|l| l.contains("Type to search")));
}

#[test]
fn results_are_grouped_and_drawn_at_every_size() {
    for size in SIZES {
        let a = answered(size);
        let s = screen(&a);
        for l in &s {
            assert!(l.chars().count() <= size.0 as usize, "{size:?}: {l:?}");
        }
        if size.1 < 10 {
            continue; // 24x8: no panic, as much as fits
        }
        let all = s.join("\n");
        assert!(all.contains("Search"), "{size:?}\n{all}");
        if size.0 >= 80 {
            for want in ["Sessions", "Projects", "Activity", "box: 2 results.", "api tests"] {
                assert!(all.contains(want), "{size:?} wants {want}\n{all}");
            }
        }
    }
    let a = answered((140, 40));
    let all = screen(&a).join("\n");
    let order: Vec<usize> = [
        "Sessions",
        "Projects",
        "Activity",
        "box: 2 results.",
        "Terminal",
        "Chat fix deploy",
    ]
    .iter()
    .map(|w| all.find(w).unwrap_or_else(|| panic!("{w}\n{all}")))
    .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}\n{all}");
    assert!(all.contains("Search everything: deploy"));
    assert!(all.contains("lab is offline"));
    assert!(all.contains("2026-10-18 14:02"));
    // The match is highlighted.
    let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
    t.draw(|f| view::draw(&a, f)).unwrap();
    let buf = t.backend().buffer().clone();
    let yellow = (0..40u16)
        .any(|y| (0..140u16).any(|x| buf[(x, y)].fg == ratatui::style::Color::Yellow && buf[(x, y)].symbol() == "d"));
    assert!(yellow);
}

#[test]
fn small_screens_get_a_plain_list() {
    let a = answered((44, 12));
    assert!(screen(&a).iter().any(|l| l.contains("Search everything: deploy")));
    let a = answered((30, 9));
    let s = screen(&a);
    assert!(s.iter().any(|l| l.starts_with("Search: deploy")), "{s:#?}");
    assert!(s.iter().any(|l| l.contains("▶ api tests")), "{s:#?}");
    assert!(!s.iter().any(|l| l.contains("Search everything")), "{s:#?}");
}

#[test]
fn late_answers_are_dropped() {
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "old");
    fire(&mut a);
    typed(&mut a, "er");
    fire(&mut a);
    a.on_search(1, Target::Server, Ok((200, server_answer())));
    assert_eq!(a.search.as_ref().unwrap().server, Some(Answer::Searching));
}

#[test]
fn an_offline_machine_is_searched_when_it_comes_back() {
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "deploy");
    fire(&mut a);
    a.on_machines(&[
        json!({"id":"m1","name":"box","online":true}),
        json!({"id":"m2","name":"lab","online":true}),
    ]);
    let r = fire(&mut a);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].target, Target::Machine("m2".into()));
    assert_eq!(r[0].path, "/api/search/machines/m2?q=deploy");
    assert!(fire(&mut a).is_empty(), "asked once");
}

#[test]
fn refusals_and_machine_states_are_sentences() {
    let mut a = app((140, 40));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "x");
    fire(&mut a);
    let g = gen(&a);
    a.on_search(
        g,
        Target::Server,
        Ok((429, json!({"error":{"code":"rate_limited","message":"<b>raw</b>"}}))),
    );
    a.on_search(g, Target::Machine("m1".into()), Ok((200, json!({"state":"off"}))));
    let all = screen(&a).join("\n");
    assert!(
        all.contains("Too many searches at once. Try again in a moment."),
        "{all}"
    );
    assert!(all.contains("box: History is off for every project (Settings → Search history)."));
    assert!(!all.contains("<b>raw"), "server text is never shown");
    for (status, code, want) in [
        (Some(400), "invalid_query", "A search needs 1 to 200 characters."),
        (Some(403), "insufficient_scope", "may not search"),
        (None, "", "No connection"),
        (Some(500), "", "did not work"),
    ] {
        assert!(search::problem(status, code).contains(want), "{status:?} {code}");
    }
    let old = search::machine_status("box", &Answer::Old);
    assert_eq!(old, "box: update its runner to search its history.");
    let partial = Answer::Done {
        hits: vec![],
        partial: true,
        more: false,
    };
    assert_eq!(search::machine_status("box", &partial), "box: nothing found in 5 s.");
    a.on_search(g, Target::Machine("m1".into()), Err("refused".into()));
    assert!(screen(&a).join("\n").contains("box didn't answer. No connection"));
    // Garbage in a 200 is a failure, not a crash.
    a.on_search(g, Target::Server, Ok((200, json!("nope"))));
    a.on_search(g, Target::Machine("m1".into()), Ok((200, json!({"state":"???"}))));
    screen(&a);
}

#[test]
fn nothing_found_says_so() {
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "zz");
    fire(&mut a);
    let g = gen(&a);
    a.on_search(
        g,
        Target::Server,
        Ok((200, json!({"query":"zz","hits":[],"truncated":false}))),
    );
    a.on_search(
        g,
        Target::Machine("m1".into()),
        Ok((200, json!({"state":"done","hits":[]}))),
    );
    let all = screen(&a).join("\n");
    assert!(all.contains("Nothing found for \"zz\"."), "{all}");
    assert!(all.contains("box: nothing found."));
}

#[test]
fn hostile_text_and_links_are_harmless() {
    let evil = format!("\u{1b}[2J\u{202e}evil\u{200b}\r\n{}", "長".repeat(3000));
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "x");
    fire(&mut a);
    let g = gen(&a);
    let hits: Vec<Value> = vec![
        json!({"kind":"session","title":evil,"snippet":evil,"match":[5,999999],"machineName":evil,"at":evil,"href":"https://evil.example/x"}),
        json!({"kind":"session","title":"t","snippet":"😀deploy","match":[1,3],"href":"//evil.example"}),
        json!({"kind":"session","title":"t2","snippet":"s","match":"x","href":"/machines/m1?pane=..%2F..%0A"}),
        json!({"kind":"weird","title":"dropped"}),
        json!({"kind":"session","title":7,"snippet":[1]}),
        json!({"kind":"project","title":"web","snippet":"s","href":"/settings/search"}),
    ];
    a.on_search(g, Target::Server, Ok((200, json!({"hits":hits}))));
    let v = a.search.as_ref().unwrap();
    let hs = v.hits();
    assert_eq!(hs.len(), 4, "unknown kinds and empty hits are dropped");
    for h in &hs {
        for s in [&h.title, &h.before, &h.matched, &h.after, &h.place] {
            assert!(
                !s.chars().any(|c| c.is_control() || c == '\u{202e}' || c == '\u{200b}'),
                "{s:?}"
            );
            assert!(s.chars().count() <= 1000);
        }
    }
    assert_eq!(hs[0].jump, None, "a whole URL is never opened");
    assert!(hs[0].matched.is_empty(), "an offset past the end voids the match");
    assert_eq!(hs[1].jump, None, "a protocol-relative link is never opened");
    assert!(
        hs[1].matched.is_empty(),
        "an offset inside a surrogate pair voids the match"
    );
    assert_eq!(hs[2].jump, None, "a pane id with path characters is refused");
    assert_eq!(hs[3].jump, Some(Jump::Web("/settings/search".into())));
    for size in SIZES {
        a.size = size;
        for l in screen(&a) {
            assert!(l.chars().count() <= size.0 as usize);
        }
    }
    // UTF-16 offsets after an astral character land on the right letters.
    let (hits, _) = search::decode_server(&json!({"hits":[
        {"kind":"session","title":"t","snippet":"😀deploy","match":[2,8]}
    ]}))
    .unwrap();
    assert_eq!((hits[0].before.as_str(), hits[0].matched.as_str()), ("😀", "deploy"));
    // A link with + and percent-encoding decodes as URLSearchParams would.
    assert_eq!(
        search::jump_of("/machines/m1?chat=a.b-1&find=two+words"),
        Some(Jump::Chat {
            machine: "m1".into(),
            chat: "a.b-1".into()
        })
    );
}

#[test]
fn arrows_and_enter_open_a_pane_at_the_line() {
    let mut a = answered((140, 40));
    // Feed p1 a long history with the line in it, far up.
    let mut bytes = Vec::new();
    for i in 0..200 {
        if i == 50 {
            bytes.extend_from_slice(b"make deploy FAILED at step 3\r\n");
        } else {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
    }
    a.on_data("m1", frame(DataKind::Snapshot, "w1:p1", 0, b"\x1b[H\x1b[2J"));
    a.on_data("m1", frame(DataKind::Output, "w1:p1", 0, &bytes));
    // Hits: Sessions(1), Projects(1), Activity(1), then box: Terminal, Chat.
    for _ in 0..3 {
        a.on_key(key(KeyCode::Down));
    }
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.focus.as_deref(), Some("w1:p1"));
    assert!(a.search_find.is_some());
    a.tick();
    let Mode::Scroll(p) = &a.mode else {
        panic!("{:?}", a.mode)
    };
    assert_eq!(p, "w1:p1");
    let shown = a.term("w1:p1").unwrap().emu.screen().contents();
    assert!(shown.contains("make deploy FAILED"), "{shown}");
    assert!(a.scroll_pos("w1:p1") > 0);
    // Back to search (kept as it was); Esc closes without opening.
    a.mode = Mode::Normal;
    prefixed(&mut a, KeyCode::Char('/'));
    assert_eq!(a.mode, Mode::Search);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
}

#[test]
fn a_line_no_longer_here_is_said() {
    let mut a = answered((140, 40));
    a.on_data("m1", frame(DataKind::Snapshot, "w1:p1", 0, b"nothing like it"));
    for _ in 0..3 {
        a.on_key(key(KeyCode::Down));
    }
    a.on_key(key(KeyCode::Enter));
    a.tick();
    assert_ne!(a.mode, Mode::Search);
    assert!(!matches!(a.mode, Mode::Scroll(_)));
    assert_eq!(
        a.note.as_deref(),
        Some("That line is no longer in this terminal's history here.")
    );
}

#[test]
fn enter_opens_a_chat_a_project_or_a_session() {
    // Up from the top wraps to the last hit: the chat.
    let mut a = answered((140, 40));
    a.on_key(key(KeyCode::Up));
    a.on_key(key(KeyCode::Enter));
    let c = a.chat.as_ref().expect("the chat is open");
    assert_eq!((c.mid.as_str(), c.sid.as_str()), ("m1", "sess-1"));
    assert!(a
        .out
        .iter()
        .any(|(m, o)| m == "m1" && matches!(o, Out::Text(v) if v["sessionId"] == "sess-1")));

    let mut a = answered((140, 40));
    a.on_key(key(KeyCode::Enter)); // the session: pane w1:p2
    assert_eq!(a.focus.as_deref(), Some("w1:p2"));
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.search_find.is_none(), "a session hit has no line to find");

    let mut a = answered((140, 40));
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Enter)); // the project w2
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.open_urls.is_empty());
}

#[test]
fn a_result_on_an_unknown_machine_opens_in_the_browser() {
    let mut a = app((80, 24));
    prefixed(&mut a, KeyCode::Char('/'));
    typed(&mut a, "x");
    fire(&mut a);
    let g = gen(&a);
    a.on_search(
        g,
        Target::Server,
        Ok((
            200,
            json!({"hits":[{"kind":"session","title":"far","snippet":"x","href":"/machines/m9?pane=w1%3Ap1"}]}),
        )),
    );
    a.on_key(key(KeyCode::Enter));
    assert_eq!(
        a.open_urls,
        vec!["https://tmuxedo.example/machines/m9?pane=w1%3Ap1".to_string()]
    );
    assert_eq!(a.note.as_deref(), Some("Opening it in your browser."));
}

#[test]
fn the_overlay_and_key_sheet_list_search() {
    let a = app((140, 40));
    for sheet in [false, true] {
        let gs = super::overlay::groups(&a.keymap, &a.overlay_ctx(), sheet);
        let it = gs
            .iter()
            .flat_map(|g| &g.items)
            .find(|i| i.pick == super::overlay::Pick::Search);
        let it = it.expect("search everything is listed");
        assert_eq!(
            (it.key.as_str(), it.label.as_str(), it.disabled),
            ("/", "search everything", None)
        );
    }
}

fn frame(kind: DataKind, pane: &str, offset: u64, bytes: &[u8]) -> DataFrame {
    DataFrame {
        kind,
        gap: false,
        req: if kind == DataKind::Snapshot { 1 } else { 0 },
        pane_id: pane.into(),
        offset,
        payload: bytes.to_vec(),
    }
}
