//! Project env (secrets-and-env.md §2.1, D-7) end to end: `config.projects`
//! reaches a new pane started in the repository (spawn env), and a bash
//! shell's cd hook exports the project's variables on entering it and
//! restores the machine's on leaving (pane > project > machine > user).

use crate::common::{options, start_holder, FakeGateway, H};
use serde_json::json;
use std::time::Duration;
use tmuxedo_runner::daemon;

const S: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_env_follows_the_repository() {
    let dir = crate::common::tempdir();
    let repo = dir.path().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@github.com:Org/Shop.git\n",
    )
    .unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.shell = Some(vec!["bash".into(), "-i".into()]);
    o.home = home.display().to_string();
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    c.send(
        json!({"t":"config","shell":{"aliases":{},"env":{"EDITOR":"vim","LANG_X":"C"},"rc":""},
        "projects":{"github.com/org/shop":{"EDITOR":"nano","NODE_ENV":"development","LANG_X":null,"PATH":"/evil"}}}),
    )
    .await;
    let h = H::attach_to(&sock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let show = "echo \"E=$EDITOR N=${NODE_ENV-unset} L=${LANG_X-unset}\"";

    // A new pane started inside the repository gets the project's env at spawn.
    let r = h
        .ok(
            "workspace.create",
            json!({"cwd": repo.join("src").to_string_lossy(), "env": {"HOME": home.to_string_lossy()}}),
        )
        .await;
    let pane = r["root_pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&pane).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": format!("{show}; echo ONE-$((1+1))\n")}),
    )
    .await;
    let t = h.wait_text(&pane, "ONE-2").await;
    assert!(t.contains("E=nano N=development L=unset"), "{t}");
    // Leaving the repository: the machine's values come back; entering again: the project's.
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": format!("cd {}\n{show}; echo TWO-$((2+2))\n", elsewhere.display())}),
    )
    .await;
    let t = h.wait_text(&pane, "TWO-4").await;
    assert!(t.contains("E=vim N=unset L=C"), "{t}");
    h.ok(
        "pane.send_text",
        json!({"pane_id": pane, "text": format!("cd {}\n{show}; echo SIX-$((3+3))\n", repo.display())}),
    )
    .await;
    let t = h.wait_text(&pane, "SIX-6").await;
    assert!(t.matches("E=nano N=development L=unset").count() >= 2, "{t}");
    // PATH is runner-managed: a project cannot set it.
    h.ok("pane.send_text", json!({"pane_id": pane, "text": "case \":$PATH:\" in *:/evil:*) echo P=evil;; *) echo P=ok;; esac; echo SEVEN-$((3+4))\n"}))
        .await;
    let t = h.wait_text(&pane, "SEVEN-7").await;
    assert!(t.contains("P=ok"), "{t}");
    // The local method the hook calls (local socket only, like hook.event).
    let r = h
        .raw("project.env", json!({"cwd": repo.to_string_lossy(), "previous": []}))
        .await;
    assert_eq!(r["result"]["project"], "github.com/org/shop");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
