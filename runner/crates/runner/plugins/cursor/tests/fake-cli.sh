#!/bin/sh
# Scripted stand-in for Cursor's `agent`, run by `tmuxedo plugin test`
# THROUGH the generated shim with the real `tmuxedo-runner hook`
# (plugins.md §11 step 4). Prints the argv and NO_OPEN_BROWSER it received,
# reads the hook command from the `--plugin-dir` plugin's hooks/hooks.json
# (as Cursor would), then drives sessionStart → beforeShellExecution →
# afterShellExecution → stop → sessionEnd.
printf 'ARGV:'
for a; do printf '<%s>' "$a"; done
echo
echo "ENV:NO_OPEN_BROWSER=${NO_OPEN_BROWSER-unset}"
dir=""
while [ $# -gt 0 ]; do
  case "$1" in
    --plugin-dir) dir="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -f "$dir/.cursor-plugin/plugin.json" ] || { echo "FAKE: no plugin manifest in $dir"; exit 3; }
[ -f "$dir/hooks/hooks.json" ] || { echo "FAKE: no hooks.json in $dir"; exit 3; }
cmd=$(sed -n 's/.*"command": "\([^"]*\)".*/\1/p' "$dir/hooks/hooks.json" | head -n 1)
[ -n "$cmd" ] || { echo "FAKE: no hook command"; exit 3; }
hook() { printf '%s' "$1" | eval "$cmd"; }
cid=0f7c-fake-cursor
hook '{"hook_event_name":"sessionStart","conversation_id":"'$cid'","session_id":"'$cid'","workspace_roots":["'"$PWD"'"]}'
hook '{"hook_event_name":"beforeShellExecution","conversation_id":"'$cid'","command":"npm install","cwd":"'"$PWD"'","sandbox":false}'
hook '{"hook_event_name":"afterShellExecution","conversation_id":"'$cid'","command":"npm install","output":"ok"}'
hook '{"hook_event_name":"stop","conversation_id":"'$cid'","status":"completed"}'
hook '{"hook_event_name":"sessionEnd","conversation_id":"'$cid'"}'
echo "FAKE DONE"
