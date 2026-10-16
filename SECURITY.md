# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue or pull request.

Use the contact form at **https://tmuxedo.dev/support** and start your message with "SECURITY".

Include what you found, how to reproduce it, the runner version (`tmuxedo-runner version`) and your OS.
We will acknowledge your report, keep you informed while we fix it, and credit you when the fix ships
unless you prefer otherwise. Please give us a reasonable time to fix the problem before you disclose it.

## Scope

In scope: the code in this repository (the runner, holder, CLI and terminal UI, hook shim, the protocol
package and its fixtures, and tmuxedo for Mac with its privileged helper) and the release binaries and
packages built from it.

Problems in the tmuxedo service itself (tmuxedo.dev: the web app, gateway, accounts) are welcome through
the same form.

Out of scope: findings that need an attacker who already controls your user account on the machine the
runner runs on, and denial of service by volume.

## What the runner trusts

The runner acts as the user it runs as, on behalf of the tmuxedo server it enrolled with. Until end-to-end
encryption and signed releases ship, that server (and its operator) can see your terminal traffic, send
input to your panes and, while `auto_update` is on, replace the runner binary. The tmuxedo security model
document describes this in full, and what changes when. If you need to rule out runner updates, build the
runner yourself and set `auto_update = false` in `~/.config/tmuxedo/runner.toml`.
