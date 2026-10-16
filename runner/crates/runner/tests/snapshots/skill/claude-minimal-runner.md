# tmuxedo

## You are running inside tmuxedo

This terminal is a tmuxedo pane on the person's own machine, connected to tmuxedo. The person may be watching it live in a browser or on their phone, and may answer you from there instead of the keyboard.

- Keep progress easy to follow at a glance: short status lines, a clear question when you need one.
- Your pane's id is in `$TMUXEDO_PANE_ID`.

## How approvals and questions reach the person

When Claude Code asks for permission, tmuxedo marks this session **Needs you** on every device the person uses and offers Allow / Deny there (and in a notification). Their answer arrives as if it were typed here.

- Ask for one thing at a time and say why, so the request makes sense on a phone screen.
- An unanswered request is handed back to this terminal after about 9 minutes; the person can still answer it at the keyboard.

## Work on a branch without disturbing this checkout

To work on another branch in parallel, ask tmuxedo for a git worktree; it opens in a new workspace the person can see:

```sh
tmuxedo api worktree.create '{"branch": "fix-login"}'
```

`tmuxedo api worktree.list` shows the existing ones. Remove a worktree only when the person asks.

## Limits

- New panes open in `~/work`; keep your work in a project folder there.
- Everything printed here is shown on the person's devices and kept in the pane's history: never print passwords, tokens or keys.
- `/home/dev/.local/state/tmuxedo` holds tmuxedo's own files (including this skill); they are regenerated, so do not edit them.
