#!/bin/sh
# Scripted stand-in for `codex`, run by `tmuxedo plugin test` THROUGH the
# generated shim with the real `tmuxedo-runner hook` (plugins.md §11 step 4).
# Prints the argv it received, takes the hook command from the first
# `-c hooks.<Event>=[…]` it was given (as Codex would), then drives
# SessionStart → UserPromptSubmit → PermissionRequest (held until the harness
# decides) → PostToolUse → Stop → SessionEnd and prints each hook's stdout.
printf 'ARGV:'
for a; do printf '<%s>' "$a"; done
echo
cmd=""
while [ $# -gt 0 ]; do
  case "$1" in
    -c)
      case "$2" in
        hooks.*=*) [ -n "$cmd" ] || cmd=$(printf '%s' "$2" | sed -n 's/.*command="\(.*\)"}]}]$/\1/p') ;;
      esac
      shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$cmd" ] || { echo "FAKE: no hooks given with -c"; exit 3; }
hook() { printf '%s' "$1" | eval "$cmd"; }
sid=019a-fake-codex
hook '{"hook_event_name":"SessionStart","session_id":"'$sid'","cwd":"'"$PWD"'","source":"startup","model":"m","permission_mode":"default"}'
hook '{"hook_event_name":"UserPromptSubmit","session_id":"'$sid'","turn_id":"t1","prompt":"x"}'
out=$(hook '{"hook_event_name":"PermissionRequest","session_id":"'$sid'","turn_id":"t1","tool_name":"Bash","tool_input":{"command":"touch x","description":"needs approval"}}')
echo "DECISION:$out"
hook '{"hook_event_name":"PostToolUse","session_id":"'$sid'","tool_name":"Bash"}'
hook '{"hook_event_name":"Stop","session_id":"'$sid'","turn_id":"t1"}'
hook '{"hook_event_name":"SessionEnd","session_id":"'$sid'","reason":"other"}'
echo "FAKE DONE"
