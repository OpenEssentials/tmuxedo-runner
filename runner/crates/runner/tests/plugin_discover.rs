//! `plugins.discover`: names only of what the person set up in each CLI's
//! own files; no command, URL or value is returned.

use serde_json::json;
use tmuxedo_runner::plugin_discover::discover;

#[test]
fn names_only_from_each_clis_own_files() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    std::fs::write(
        h.join(".claude.json"),
        json!({
            "mcpServers": {"github": {"command": "npx", "env": {"GITHUB_TOKEN": "value-not-returned"}}},
            "projects": {"/work/app": {"mcpServers": {"postgres": {"command": "pg"}}}}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(h.join(".claude/skills/release-notes")).unwrap();
    std::fs::write(h.join(".claude/skills/release-notes/SKILL.md"), "---\nname: r\n---\n").unwrap();
    std::fs::create_dir_all(h.join(".claude/skills/no-skill-file")).unwrap();
    std::fs::create_dir_all(h.join(".claude/agents")).unwrap();
    std::fs::write(h.join(".claude/agents/reviewer.md"), "x").unwrap();
    std::fs::write(h.join(".claude/agents/notes.txt"), "x").unwrap();
    let codex = h.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("config.toml"),
        "model = \"x\"\n[mcp_servers.linear]\nurl = \"https://mcp.example.com\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(h.join(".cursor")).unwrap();
    std::fs::write(h.join(".cursor/mcp.json"), "{not json").unwrap();

    let before = std::fs::read(h.join(".claude.json")).unwrap();
    let v = discover(h, &codex);
    assert_eq!(
        v,
        json!({"type": "plugins_discover", "plugins": {
            "claude": {"mcp": ["github", "postgres"], "skill": ["release-notes"], "subagent": ["reviewer"]},
            "codex": {"mcp": ["linear"]}
        }})
    );
    assert!(!v.to_string().contains("value-not-returned"));
    assert_eq!(
        std::fs::read(h.join(".claude.json")).unwrap(),
        before,
        "read, never written"
    );
    assert_eq!(
        discover(tempfile::tempdir().unwrap().path(), &codex.join("none")),
        json!({"type": "plugins_discover", "plugins": {}})
    );
}
