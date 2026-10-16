//! Property tests: the condition language, scopes, readings and identification
//! never panic, and behave like the specification says on arbitrary input.

use proptest::prelude::*;
use tmuxedo_detect::cond::{Cond, Subject};
use tmuxedo_detect::scope::Scope;
use tmuxedo_detect::{explain, identify, read, Agent, Input, Job, Process, Status};

/// Screen-like text: lines of words from agent UIs, box drawing and spinners.
fn screen() -> impl Strategy<Value = String> {
    let word = prop_oneof![
        Just("esc to interrupt".to_string()),
        Just("esc to cancel".to_string()),
        Just("Do you want to proceed?".to_string()),
        Just("❯ 1. Yes".to_string()),
        Just("─".repeat(20)),
        Just("› ".to_string()),
        Just("• done".to_string()),
        Just("✻ Thinking… (3s · esc to interrupt)".to_string()),
        Just("⠋ Working".to_string()),
        Just("漢字テキスト".to_string()),
        "[ -~]{0,30}",
        "\\PC{0,12}",
    ];
    prop::collection::vec(word, 0..16).prop_map(|v| v.join("\n"))
}

/// Random condition trees over a small alphabet.
fn cond() -> impl Strategy<Value = Cond> {
    let leaf = prop_oneof![
        "[a-c]{1,2}".prop_map(|t| Cond::parse(&format!("\"{t}\"")).unwrap()),
        "[a-c]{1,2}".prop_map(|t| Cond::parse(&format!("/{t}/")).unwrap()),
        "[a-c]{1,2}".prop_map(|t| Cond::parse(&format!("line /^{t}/")).unwrap()),
        Just(Cond::parse(r#""q\"\\""#).unwrap()),
        Just(Cond::parse(r"/x\/y/").unwrap()),
    ];
    leaf.prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|c| Cond::Not(Box::new(c))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Cond::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner).prop_map(|(a, b)| Cond::Or(Box::new(a), Box::new(b))),
        ]
    })
}

fn text() -> impl Strategy<Value = String> {
    "[abcqxy/\"\\\\\n]{0,12}"
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn the_parser_never_panics(src in "\\PC{0,60}") {
        let _ = Cond::parse(&src);
    }

    #[test]
    fn the_parser_never_panics_on_language_like_input(src in "[\"/()a-z \\\\\n]{0,60}") {
        if let Ok(c) = Cond::parse(&src) {
            // Whatever parses prints canonically and reparses to the same thing.
            let shown = c.to_string();
            prop_assert_eq!(Cond::parse(&shown).unwrap().to_string(), shown);
        }
    }

    #[test]
    fn canonical_form_round_trips_and_evaluates_the_same(c in cond(), t in text()) {
        let shown = c.to_string();
        let back = Cond::parse(&shown).unwrap();
        prop_assert_eq!(back.to_string(), shown.clone());
        let s = Subject::new(&t);
        prop_assert_eq!(back.eval(&s), c.eval(&s), "{}", shown);
    }

    #[test]
    fn boolean_algebra_holds(a in cond(), b in cond(), t in text()) {
        let s = Subject::new(&t);
        let (x, y) = (a.eval(&s), b.eval(&s));
        prop_assert_eq!(Cond::Not(Box::new(a.clone())).eval(&s), !x);
        prop_assert_eq!(Cond::And(Box::new(a.clone()), Box::new(b.clone())).eval(&s), x && y);
        prop_assert_eq!(Cond::Or(Box::new(a.clone()), Box::new(b.clone())).eval(&s), x || y);
        // De Morgan.
        let lhs = Cond::Not(Box::new(Cond::And(Box::new(a.clone()), Box::new(b.clone()))));
        let rhs = Cond::Or(Box::new(Cond::Not(Box::new(a))), Box::new(Cond::Not(Box::new(b))));
        prop_assert_eq!(lhs.eval(&s), rhs.eval(&s));
    }

    #[test]
    fn text_terms_ignore_case(needle in "[a-zA-Zäöü]{1,6}", hay in "[a-zA-Zäöü ]{0,20}") {
        let c = Cond::parse(&format!("\"{needle}\"")).unwrap();
        let want = hay.to_lowercase().contains(&needle.to_lowercase());
        prop_assert_eq!(c.eval(&Subject::new(&hay)), want);
        prop_assert_eq!(c.eval(&Subject::new(&hay.to_uppercase())), hay.to_uppercase().to_lowercase().contains(&needle.to_lowercase()));
    }

    #[test]
    fn scopes_are_slices_of_their_input(s in screen(), n in 1usize..40) {
        let input = Input { screen: &s, title: "t", progress: "p" };
        for scope in [
            Scope::Screen, Scope::Last(n), Scope::First(n), Scope::BelowRule, Scope::Box, Scope::AboveBox,
            Scope::AboveBoxLine, Scope::AfterComposer, Scope::BeforeComposer, Scope::UnlessComposer,
        ] {
            let part = scope.slice(&input);
            let (base, end) = (s.as_ptr() as usize, s.as_ptr() as usize + s.len());
            let p = part.as_ptr() as usize;
            prop_assert!(part.is_empty() || (p >= base && p + part.len() <= end), "{scope} escaped its input");
        }
        // last N grows with N and always ends the screen; first N always starts it.
        let a = Scope::Last(n).slice(&input);
        let b = Scope::Last(n + 1).slice(&input);
        prop_assert!(b.len() >= a.len());
        prop_assert!(s.ends_with(a) || a.is_empty());
        prop_assert!(s.starts_with(Scope::First(n).slice(&input)));
    }

    #[test]
    fn readings_never_panic_and_agree_with_explain(s in screen(), title in "\\PC{0,8}", progress in "[0-9;-]{0,6}") {
        let input = Input { screen: &s, title: &title, progress: &progress };
        for agent in Agent::all() {
            let r = read(agent, &input);
            let e = explain(agent, &input);
            prop_assert_eq!(r, e.status, "{}", agent.id());
            prop_assert_eq!(e.default, e.signal.is_none());
            if e.default {
                prop_assert_eq!(r, Status::Idle);
            }
        }
    }

    #[test]
    fn identification_never_panics(
        names in prop::collection::vec("[a-z./-]{0,12}", 1..4),
        argvs in prop::collection::vec(prop::collection::vec("[a-z./@=-]{0,24}", 0..5), 1..4),
        pgid in 0u32..5,
    ) {
        let processes: Vec<Process> = names
            .iter()
            .zip(argvs)
            .enumerate()
            .map(|(i, (n, argv))| Process { pid: i as u32, name: n.clone(), argv })
            .collect();
        let job = Job { pgid, processes };
        let _ = identify(&job);
    }
}
