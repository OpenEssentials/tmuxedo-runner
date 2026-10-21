//! `prefix F` (super::browse, Phase 15b): the read-only file browser. The
//! requests the event loop sends, decoding (a wrong answer, a name that is
//! not one path component, text that would move the cursor), refusals as
//! plain sentences, navigation in and back out, and TestBackend snapshots.

use super::app::{App, Mode};
use super::browse::{self, Load, Place};
use super::view;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};

fn app() -> App {
    let mut a = App::new(true, (100, 24));
    a.on_machines(&[json!({"id":"m1","name":"box","online":true})]);
    a.select_initial(None);
    a
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn open(a: &mut App) {
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char('F')));
}

fn render(app: &App) -> String {
    let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
    t.draw(|f| view::draw(app, f)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Answer the one request waiting and return its path.
fn answer(a: &mut App, r: Result<Value, (Option<u16>, String)>) -> String {
    let (mid, req) = a.files_out.pop().expect("a request");
    assert!(a.files_out.is_empty());
    a.on_files(&mid, &req, r);
    req
}

fn roots() -> Value {
    json!({"type":"fs_roots","canWrite":true,"roots":[{"id":"r1","kind":"workspace","label":"site","writable":true},{"id":"bad id","label":"x"}],
           "refused":[{"label":"home","why":"too_broad"}]})
}

fn list(path: &str) -> Value {
    json!({"type":"fs_list","root":"r1","path":path,"hidden":2,"next":"f/readme.md","entries":[
        {"name":"readme.md","kind":"file","size":2150},{"name":"docs","kind":"dir"},{"name":"../up","kind":"file"},
        {"name":"evil\u{1b}[2J\u{202E}gpj.exe","kind":"file","size":1},{"name":"current","kind":"link"}]})
}

#[test]
fn opens_on_the_places_and_walks_in_and_out() {
    let mut a = app();
    open(&mut a);
    assert_eq!(a.mode, Mode::Files);
    assert_eq!(answer(&mut a, Ok(roots())), "/api/v1/machines/m1/fs/roots");
    let s = render(&a);
    assert!(s.contains("Files on box (read only)"), "{s}");
    assert!(s.contains("▶ site"), "{s}");
    assert!(s.contains("home is too broad to browse"), "{s}");
    assert!(!s.contains("bad id"));

    a.on_key(key(KeyCode::Enter));
    assert_eq!(
        answer(&mut a, Ok(list(""))),
        "/api/v1/machines/m1/fs/list?root=r1&path="
    );
    let s = render(&a);
    assert!(s.contains("Files on box › site (read only)"), "{s}");
    // Folders first; a name that is not one component is dropped; hidden count; more.
    let docs = s.find("▶ docs/").expect("docs");
    let readme = s.find("readme.md").expect("readme");
    assert!(docs < readme, "{s}");
    assert!(s.contains("2.1 KB"), "{s}");
    assert!(!s.contains("../up"));
    assert!(s.contains("↪ current (link, not followed)"), "{s}");
    assert!(s.contains("2 items hidden: they may hold secrets"), "{s}");
    assert!(s.contains("m shows more"), "{s}");
    assert!(
        !s.contains('\u{1b}') && !s.contains('\u{202E}'),
        "control and bidi characters never reach the screen"
    );

    // Into docs/, then back up: the cursor lands on docs again.
    a.on_key(key(KeyCode::Enter));
    assert_eq!(
        answer(&mut a, Ok(list("docs"))),
        "/api/v1/machines/m1/fs/list?root=r1&path=docs"
    );
    assert!(render(&a).contains("› site › docs (read only)"));
    a.on_key(key(KeyCode::Left));
    assert_eq!(
        answer(&mut a, Ok(list(""))),
        "/api/v1/machines/m1/fs/list?root=r1&path="
    );
    assert_eq!(a.files.as_ref().unwrap().cursor, 0);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Backspace));
    answer(&mut a, Ok(roots()));
    assert_eq!(a.files.as_ref().unwrap().place, Place::Roots);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
    assert!(a.files.is_none());
}

#[test]
fn reads_a_file_line_by_line_and_scrolls() {
    let mut a = app();
    open(&mut a);
    answer(&mut a, Ok(roots()));
    a.on_key(key(KeyCode::Enter));
    answer(&mut a, Ok(list("")));
    // docs/, current, evil…, readme.md: the last row.
    a.on_key(key(KeyCode::End));
    a.on_key(key(KeyCode::Enter));
    let text: String = (1..=40).map(|i| format!("line {i}\t\u{1b}[31mred\r\n")).collect();
    let req = answer(
        &mut a,
        Ok(json!({"type":"fs_file","root":"r1","path":"readme.md","kind":"text","text":text,"masked":true,"size":1})),
    );
    assert_eq!(req, "/api/v1/machines/m1/fs/read?root=r1&path=readme.md");
    let s = render(&a);
    assert!(s.contains("Parts that looked like secrets are hidden."), "{s}");
    assert!(s.contains("line 1    �[31mred"), "{s}");
    assert!(s.contains("PgUp PgDn scroll"), "{s}");
    a.on_key(key(KeyCode::PageDown));
    let s = render(&a);
    assert!(s.contains("line 11 ") && !s.contains("line 1 "), "{s}");
    // Back out to the folder, the file selected.
    a.on_key(key(KeyCode::Left));
    answer(&mut a, Ok(list("")));
    let v = a.files.as_ref().unwrap();
    assert_eq!(v.cursor, 3);
}

#[test]
fn show_more_joins_the_next_page() {
    let mut a = app();
    open(&mut a);
    answer(&mut a, Ok(roots()));
    a.on_key(key(KeyCode::Enter));
    answer(&mut a, Ok(list("")));
    a.on_key(key(KeyCode::Char('m')));
    let req = answer(
        &mut a,
        Ok(
            json!({"type":"fs_list","root":"r1","path":"","hidden":0,"entries":[{"name":"z.txt","kind":"file","size":3}]}),
        ),
    );
    assert_eq!(req, "/api/v1/machines/m1/fs/list?root=r1&path=&cursor=f%2Freadme.md");
    let Load::Folder { entries, next, .. } = &a.files.as_ref().unwrap().load else {
        panic!()
    };
    assert!(entries.iter().any(|e| e.name == "z.txt") && entries.iter().any(|e| e.name == "docs"));
    assert!(next.is_none());
    assert!(!render(&a).contains("m shows more"));
}

#[test]
fn refusals_are_plain_sentences_and_wrong_answers_are_dropped() {
    let mut a = app();
    open(&mut a);
    answer(&mut a, Err((Some(409), "machine_offline".into())));
    let s = render(&a);
    assert!(s.contains("The machine is not connected."), "{s}");
    a.on_key(key(KeyCode::Char('r')));
    answer(&mut a, Ok(json!({"type":"fs_list","root":"r1","path":"","entries":[]})));
    assert!(render(&a).contains("could not read"));
    // An answer for a request no longer wanted changes nothing.
    a.on_key(key(KeyCode::Char('r')));
    a.on_files("m1", "/api/v1/machines/m1/fs/list?root=r1&path=", Ok(list("")));
    assert_eq!(a.files.as_ref().unwrap().load, Load::Loading);

    assert_eq!(
        browse::problem(Some(403), "file_refused"),
        "Hidden: this may hold secrets. Open it in the terminal if you need it."
    );
    assert_eq!(browse::problem(None, ""), "No connection. Try again.");
    assert_eq!(
        browse::problem(Some(502), "raw machine words"),
        "The machine could not do that."
    );
}

#[test]
fn decoding_rules() {
    // Another folder's answer, a bad cursor, and a non-text file.
    assert!(browse::decode_list(&list("x"), "r1", "").is_none());
    let bad_next = json!({"type":"fs_list","root":"r1","path":"","entries":[],"next":"../../x"});
    let Some(Load::Folder { next, .. }) = browse::decode_list(&bad_next, "r1", "") else {
        panic!()
    };
    assert!(next.is_none());
    let bin = json!({"type":"fs_file","root":"r1","path":"a.bin","kind":"binary","why":"not_utf8"});
    let Some(Load::File { notice, lines, .. }) = browse::decode_file(&bin, "r1", "a.bin") else {
        panic!()
    };
    assert!(lines.is_empty() && notice.unwrap().contains("not UTF-8"));
    assert_eq!(browse::safe("a\u{200B}b\u{2066}c\u{7}"), "a�b�c�");
    assert_eq!(
        browse::path("m 1/x", "list", &[("root", "r1"), ("path", "a b/ü")]),
        "/api/v1/machines/m%201%2Fx/fs/list?root=r1&path=a%20b%2F%C3%BC"
    );
}

#[test]
fn the_key_is_in_the_overlay() {
    let mut a = app();
    a.size = (100, 60);
    a.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
    let s = render(&a);
    assert!(s.contains("F files"), "{s}");
}
