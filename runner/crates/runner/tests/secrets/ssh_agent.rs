//! The runner's ssh-agent against real OpenSSH (secrets-and-env.md §6a.5):
//! a pane logs in and pushes with a bound key it cannot read; a key limited
//! to host A is refused for host B; confirm-each-use (deny, timeout, approve
//! once); commit signing; an unbound pane has no agent; `ssh-add` cannot
//! change keys; revoking removes the key at once; no private key material in
//! any frame or file.
//!
//! Needs `sshd` and root (the throwaway servers log in as the test user);
//! skipped otherwise (the CI test image installs openssh-server).

use crate::common::{options, start_holder, Conn, FakeGateway, H};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;
use tmuxedo_runner::daemon;
use tmuxedo_runner::secrets::seal;
use tmuxedo_runner::secrets::sshwire::{authorized_line, PrivateKey};

const S: Duration = Duration::from_secs(30);

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Sshd {
    child: std::process::Child,
    port: u16,
    host_pub: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn sshd(dir: &std::path::Path, name: &str, authorized: &std::path::Path) -> Sshd {
    let port = free_port();
    let hk = dir.join(format!("{name}_host"));
    let st = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&hk)
        .status()
        .unwrap();
    assert!(st.success());
    let cfg = dir.join(format!("{name}.conf"));
    std::fs::write(
        &cfg,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPidFile {}\nStrictModes no\nUsePAM no\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin prohibit-password\nLogLevel ERROR\n",
            hk.display(),
            authorized.display(),
            dir.join(format!("{name}.pid")).display()
        ),
    )
    .unwrap();
    let _ = std::fs::create_dir_all("/run/sshd");
    let child = std::process::Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(&cfg)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let host_pub = std::fs::read_to_string(hk.with_extension("pub")).unwrap();
    Sshd { child, port, host_pub }
}

async fn answer_ssh(c: &mut Conn, key: &str, id: &str, der: &[u8]) {
    let req = c
        .text(S, |v| v["t"] == "secret_req" && v["purpose"]["kind"] == "ssh")
        .await;
    assert_eq!(req["names"], json!([format!("ssh:{id}")]));
    let rid = req["id"].as_str().unwrap().to_string();
    let plain =
        json!({"values": {format!("ssh:{id}"): base64::engine::general_purpose::STANDARD.encode(der)}, "versions": {}});
    let sealed = seal::seal(key, plain.to_string().as_bytes(), &seal::answer_context("m1", &rid)).unwrap();
    c.send(json!({"t":"secret_res","id": rid,"result":{"sealed": sealed}}))
        .await;
}

fn policy(line: &str, dest: &[String], confirm: &str, signing: bool) -> Value {
    json!({"t":"secrets","policy":{"names":[],"offlineCache":false,"ssh":[{
        "id":"k1","name":"deploy","kind":"vault","publicKey": line,"destinations": dest,
        "confirm": confirm,"scope":"user","plugins":[],"signing": signing}]}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ssh_keys_are_usable_never_readable() {
    // SAFETY: getuid only reads.
    if !std::path::Path::new("/usr/sbin/sshd").exists() || unsafe { libc::getuid() } != 0 {
        eprintln!("skipped: needs sshd and root");
        return;
    }
    std::env::set_var("TMUXEDO_SSH_CONFIRM_TIMEOUT_MS", "1500");
    let dir = crate::common::tempdir();
    let (der, key) = PrivateKey::generate_ed25519().unwrap();
    let line = authorized_line(&key.blob(), "deploy");
    let der_b64 = base64::engine::general_purpose::STANDARD.encode(der.as_slice());
    let authorized = dir.path().join("authorized_keys");
    std::fs::write(&authorized, format!("{line}\n")).unwrap();
    let a = sshd(dir.path(), "a", &authorized);
    let b = sshd(dir.path(), "b", &authorized);
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let kh = home.join(".ssh/known_hosts");
    std::fs::write(
        &kh,
        format!(
            "[127.0.0.1]:{} {}[127.0.0.1]:{} {}",
            a.port, a.host_pub, b.port, b.host_pub
        ),
    )
    .unwrap();
    let ssh = |port: u16, cmd: &str| {
        format!(
            "ssh -F /dev/null -o BatchMode=yes -o UserKnownHostsFile={} -o StrictHostKeyChecking=yes -p {port} root@127.0.0.1 {cmd}",
            kh.display()
        )
    };

    let holder = start_holder(dir.path());
    let gw = FakeGateway::start().await;
    let mut o = options(dir.path(), &gw.url, holder);
    o.home = home.display().to_string();
    let sock = o.runner_socket.clone();
    let task = tokio::spawn(daemon::run(o));
    let mut c = gw.accept(S).await.unwrap();
    c.hello(S).await;
    let rkey = c.text(S, |v| v["t"] == "secret_key").await["publicKey"]
        .as_str()
        .unwrap()
        .to_string();
    let h = H::attach_to(&sock).await;
    let dest_a = vec![format!("[127.0.0.1]:{}", a.port)];
    c.send(policy(&line, &dest_a, "never", false)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The first pane predates the key: no agent there.
    h.ok(
        "pane.send_text",
        json!({"pane_id":"w1:p1","text":"echo SOCK=${SSH_AUTH_SOCK-none} END-$((1+1))\n"}),
    )
    .await;
    assert!(h.wait_text("w1:p1", "END-2").await.contains("SOCK=none"));

    let split = h
        .ok(
            "pane.split",
            json!({"target_pane_id":"w1:p1","direction":"right","env":{"HOME": home.to_string_lossy()}}),
        )
        .await;
    let p = split["pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&p).await;
    let run = |text: String| json!({"pane_id": p, "text": text + "\n"});

    // ssh-add shows the public half only and cannot change the agent's keys.
    h.ok(
        "pane.send_text",
        run("ssh-add -L; ssh-add -D; echo RC=$? ADD-$((2+2))".into()),
    )
    .await;
    let t = h.wait_text(&p, "ADD-4").await;
    assert!(t.contains(line.split_whitespace().nth(1).unwrap()), "{t}");
    assert!(t.contains("RC=1"), "remove-all is refused: {t}");

    // Host A: logs in with the key (fetched sealed on first use).
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RCA=$? RCA-END-$((1+1))",
            ssh(a.port, "echo LOGIN-A-$((40+2))")
        )),
    )
    .await;
    answer_ssh(&mut c, &rkey, "k1", &der).await;
    let t = h.wait_text(&p, "RCA-END-2").await;
    assert!(t.contains("LOGIN-A-42"), "{t}");
    // Host B: the same key is refused there.
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RCB=$? RCB-END-$((1+1))",
            ssh(b.port, "echo LOGIN-B-$((40+2))")
        )),
    )
    .await;
    let t = h.wait_text(&p, "RCB-END-2").await;
    assert!(!t.contains("LOGIN-B-42"), "{t}");

    // git push over ssh with the bound key.
    let remote = dir.path().join("remote.git");
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()
        .unwrap()
        .success());
    let work = dir.path().join("work");
    h.ok(
        "pane.send_text",
        run(format!(
            "git init -q {w} && cd {w} && git -c user.name=t -c user.email=t@example.test commit -q --allow-empty -m one && \
             GIT_SSH_COMMAND='ssh -F /dev/null -o BatchMode=yes -o UserKnownHostsFile={kh} -p {port}' git push -q root@127.0.0.1:{r} HEAD:main; echo PUSH=$? PUSH-END-$((1+1))",
            w = work.display(),
            kh = kh.display(),
            port = a.port,
            r = remote.display()
        )),
    )
    .await;
    assert!(h.wait_text(&p, "PUSH-END-2").await.contains("PUSH=0"));
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&remote)
        .args(["log", "--oneline", "main"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("one"));

    // Confirm each use: deny refuses, no answer refuses, allow works once.
    c.send(policy(&line, &dest_a, "each", false)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let confirm_once = |c: &mut Conn| {
        let f = c
            .texts
            .iter()
            .position(|v| v["t"] == "event" && v["type"] == "ssh_confirm");
        f.map(|i| c.texts.remove(i))
    };
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RC1=$? RC1-END-$((1+1))",
            ssh(a.port, "echo C1-$((5+5))")
        )),
    )
    .await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "ssh_confirm").await;
    assert_eq!(ev["data"]["key"], "deploy");
    assert_eq!(ev["data"]["use"], dest_a[0]);
    c.send(json!({"t":"decision","paneId": p,"promptId": ev["data"]["promptId"],"behavior":"deny"}))
        .await;
    let t = h.wait_text(&p, "RC1-END-2").await;
    assert!(t.contains("RC1=255") && !t.contains("C1-10"), "denied: {t}");
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RC2=$? RC2-END-$((1+1))",
            ssh(a.port, "echo C2-$((6+6))")
        )),
    )
    .await;
    let _ = c.text(S, |v| v["t"] == "event" && v["type"] == "ssh_confirm").await;
    let t = h.wait_text(&p, "RC2-END-2").await;
    assert!(t.contains("RC2=255") && !t.contains("C2-12"), "timed out: {t}");
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RC3=$? RC3-END-$((1+1))",
            ssh(a.port, "echo C3-$((7+7))")
        )),
    )
    .await;
    let ev = c.text(S, |v| v["t"] == "event" && v["type"] == "ssh_confirm").await;
    c.send(json!({"t":"decision","paneId": p,"promptId": ev["data"]["promptId"],"behavior":"allow"}))
        .await;
    let t = h.wait_text(&p, "RC3-END-2").await;
    assert!(t.contains("C3-14"), "approved: {t}");
    // The same prompt id cannot approve a second use.
    c.send(json!({"t":"decision","paneId": p,"promptId": ev["data"]["promptId"],"behavior":"allow"}))
        .await;
    h.ok(
        "pane.send_text",
        run(format!(
            "{}; echo RC4=$? RC4-END-$((1+1))",
            ssh(a.port, "echo C4-$((8+8))")
        )),
    )
    .await;
    let ev4 = c.text(S, |v| v["t"] == "event" && v["type"] == "ssh_confirm").await;
    assert_ne!(ev4["data"]["promptId"], ev["data"]["promptId"]);
    let t = h.wait_text(&p, "RC4-END-2").await;
    assert!(!t.contains("C4-16"), "{t}");
    let _ = confirm_once(&mut c);

    // Commit signing with the key, in a new pane.
    c.send(policy(&line, &[], "never", true)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let s2 = h
        .ok(
            "pane.split",
            json!({"target_pane_id": p, "direction":"down","env":{"HOME": home.to_string_lossy()}}),
        )
        .await;
    let p2 = s2["pane"]["pane_id"].as_str().unwrap().to_string();
    h.wait_alive(&p2).await;
    h.ok(
        "pane.send_text",
        json!({"pane_id": p2, "text": format!("cd {} && git -c user.name=t -c user.email=t@example.test commit -q --allow-empty -m signed; git cat-file commit HEAD | grep -c 'BEGIN SSH SIGNATURE'; echo SIGN-$((9+9))\n", work.display())}),
    )
    .await;
    let t = h.wait_text(&p2, "SIGN-18").await;
    // (A prompt may be drawn on the same line before the count.)
    assert!(
        t.lines().any(|l| l.trim().trim_start_matches('#').trim() == "1"),
        "a signed commit: {t}"
    );

    // Revoked: gone from the agent at once.
    c.send(json!({"t":"secrets","policy":{"names":[],"ssh":[]},"revoked":["ssh:k1"]}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.ok("pane.send_text", run("ssh-add -L; echo GONE-$((10+10))".into()))
        .await;
    let t = h.wait_text(&p, "GONE-20").await;
    assert!(t.contains("no identities"), "{t}");

    // No private key material anywhere: frames, files under home, the pane.
    c.drain(Duration::from_millis(300)).await;
    let seed_b64 = &der_b64[der_b64.len() - 20..];
    for v in c.texts.iter() {
        assert!(!v.to_string().contains(seed_b64), "a text frame held key material");
    }
    for f in c.bins.iter() {
        assert!(!String::from_utf8_lossy(&f.payload).contains(seed_b64));
    }
    let found = std::process::Command::new("find")
        .arg(&home)
        .args(["-name", "id_*"])
        .output()
        .unwrap();
    assert!(found.stdout.is_empty(), "no key files");
    c.close(4003).await;
    let _ = tokio::time::timeout(S, task).await;
}
