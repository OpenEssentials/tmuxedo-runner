#!/bin/sh
# Scripted stand-in for `claude`, run by `tmuxedo plugin test` THROUGH the
# generated shim with the real `tmuxedo-runner hook` (plugins.md §11 step 4).
# Prints the argv it received, then drives SessionStart → UserPromptSubmit →
# PermissionRequest (held until the harness decides) → PostToolUse → Stop →
# SessionEnd and prints each hook's stdout.
printf 'ARGV:'
for a; do printf '<%s>' "$a"; done
echo
settings=""
while [ $# -gt 0 ]; do
  case "$1" in
    --settings) settings="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -f "$settings" ] || { echo "FAKE: no settings file"; exit 3; }
cmd=$(sed -n 's/.*"command": "\(.*\)",$/\1/p' "$settings" | head -n 1)
hook() { printf '%s' "$1" | eval "$cmd"; }
sid=fake-session-1
hook '{"hook_event_name":"SessionStart","session_id":"'$sid'","cwd":"'"$PWD"'","source":"startup"}'
hook '{"hook_event_name":"UserPromptSubmit","session_id":"'$sid'","prompt":"x"}'
out=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","tool_name":"Bash","tool_input":{"command":"touch x"},"permission_suggestions":[]}')
echo "DECISION:$out"
hook '{"hook_event_name":"PostToolUse","session_id":"'$sid'","tool_name":"Bash"}'
hook '{"hook_event_name":"Stop","session_id":"'$sid'"}'
hook '{"hook_event_name":"SessionEnd","session_id":"'$sid'"}'
echo "FAKE DONE"
