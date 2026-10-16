# Agent detection (tmuxedo's own engine)

*Written 2026-10-05, before the code. This is the specification `runner/crates/detect` is written from. When the
code and this file disagree, one of them is a bug.*

## 1. What it does

A pane runs a shell. Sometimes the foreground program in that shell is a coding agent (Claude Code, Codex, …).
The runner wants to know two things about every pane, many times a second, cheaply:

1. **Which agent** is in the foreground, if any (from the process table).
2. **What it is doing** (from what the terminal shows): working, blocked on the human, or idle.

The detect crate answers both. It does not decide the pane's final status: the runner combines its answer with
hook reports and output timing (§11). It has no network access, no global mutable state besides one lazily built
catalogue, and no third-party code.

## 2. Statuses

The crate reports one of five readings for a screen:

| Reading | Meaning | What the runner does with it |
|---|---|---|
| `working` | The agent shows live activity (spinner, timer, "interrupt" hint, busy title). | screen status = working |
| `blocked` | The agent shows a question, approval or form that needs the human. | screen status = blocked |
| `idle` | The agent shows its input prompt with nothing running, **or** no signal matched for a known agent (the default). | screen status = idle |
| `unknown` | A signal says the screen carries no usable evidence. | screen status cleared; the runner falls back to its timing heuristic |
| `hold` | The agent shows a viewer (transcript, model picker…) that hides the live state. | the previous screen status is kept |

`done` is not a screen reading. It is the runner's own status for "finished a turn after working" (§11.2).

## 3. Inputs

`Input { screen, title, progress }`, all `&str`:

- `screen`: the pane's visible rows as plain text (the emulator's `detection_text()`): one line per row, rows joined
  by `\n`, trailing blanks trimmed per row, no escape sequences. Wide characters appear once (their spacer cell is
  dropped by the emulator).
- `title`: the last OSC 0/2 window title the program set (empty if none).
- `progress`: the payload of the last OSC 9;4 progress report, e.g. `4;1;50` (empty if none).

Lines are split on `\n`; a `\r` at the end of a line is ignored. An empty `screen` is valid.

## 4. The agent catalogue

Every supported agent has one **agent file**, TOML, compiled into the binary from `runner/crates/detect/agents/`.
The format is tmuxedo's own:

```toml
agent = "claude"                 # the canonical id: the pane's agent `kind` and the API label
title = "Claude Code"            # human name (explain output, docs)
revision = "2026-10-05.1"        # bumped when signals change; shown by server.agent_manifests
launch = "claude"                # executable `agent.start` types into the pane
names = ["claude-code"]          # more process / label names that mean this agent

[process]                        # optional, see §8
scripts = ["node_modules/@scope/pkg/dist/cli.js"]   # a runtime running this script is the agent
versioned = "muse-bin-"          # an executable named <prefix><digit>… is the agent
batch_flags = ["-p", "--print"]  # with any of these the process is a one-shot run, not the interactive agent
value_flags = ["--backend"]      # flags that take a value (skipped when looking for a subcommand)
subcommand_is_batch = true       # a first positional argument (a subcommand) means a one-shot run

[[signal]]
name = "prompt-box"              # unique within the file
status = "idle"                  # working | blocked | idle | unknown | hold
rank = 60                        # higher wins
scope = "box"                    # which part of the input to look at (§5)
when = '''line /^\s*❯/ and not "esc to cancel"'''   # the condition (§6)
```

An agent file may have no signals: the agent is then identified by process only and reads `idle` (§7).

### 4.1 Validation

A file is rejected (bundled: the build's tests fail; local override: ignored with a warning, §9) when:

- a required key (`agent`, `title`, `revision`, `launch`) is missing or empty, or an unknown key appears anywhere;
- two signals share a name, or a name is empty or longer than 64 characters;
- `status` or `scope` is not one of the listed values, or a `last N` / `first N` count is not 1…500;
- `when` does not parse (§6.1), a regex does not compile, a regex or text literal is longer than 1,000 characters;
- the file has more than 100 signals, or one condition has more than 64 terms or nests deeper than 8 levels;
- `hold` is used with a scope of `title` or `progress` (a title cannot be a viewer).

## 5. Scopes

A scope picks a slice of the input. Every scope below is a contiguous slice of the screen except `title` and
`progress`.

| Scope | Text |
|---|---|
| `screen` | the whole screen |
| `title` | the window title |
| `progress` | the OSC 9;4 payload |
| `last N` | from the N-th non-blank line counted from the bottom to the end of the screen (blank lines between them included). Fewer than N non-blank lines: from the first non-blank line. No non-blank line: empty. |
| `first N` | from the start of the screen to the end of the N-th non-blank line, including its line break. Fewer than N: up to the end of the last non-blank line. No non-blank line: empty. |
| `below-rule` | everything after the last **rule line** (below). No rule line: the whole screen. |
| `box` | the **prompt box** body: the lines after the box's top rule, up to (not including) the next rule line, or to the end. No box: empty. |
| `above-box` | everything before the prompt box's top rule. No box: the whole screen. |
| `above-box-line` | the last non-blank line of `above-box` (empty if none). |
| `after-composer` | everything after the last **composer line**. None: the whole screen. |
| `before-composer` | everything before the **live composer line**. None: the whole screen. |
| `unless-composer` | the whole screen when there is no live composer line, else empty. |

Definitions:

- A **rule line** is a line that, with surrounding whitespace removed, starts with `─` (U+2500) and either consists
  of `─` only, or starts with at least three `─` (a titled rule such as `─── Plan ───`).
- The **prompt box** is the region whose top border is the second rule line counted from the bottom.
- A **composer line** is a line that is exactly `›` (U+203A) or starts with `› `. It is **live** when it is the last
  composer line and no **reply marker** line (a line starting with `•`, `■`, `✗` or `✓`) follows it.

## 6. Conditions

`when` is a small boolean language, evaluated against the signal's scope text.

| Term | True when |
|---|---|
| `"text"` | the scope contains `text`, ignoring case (Unicode lower-casing of both sides) |
| `/regex/` | the regex matches somewhere in the scope (case-sensitive unless the regex says `(?i)`) |
| `line /regex/` | at least one line of the scope matches the regex on its own |
| `not X` | X is false |
| `X and Y` | both |
| `X or Y` | either |
| `( X )` | grouping |

Precedence: `not` binds tightest, then `and`, then `or`. `and`/`or` are left-associative and short-circuit.
Whitespace (including newlines) separates tokens.

### 6.1 Grammar

```
expr    := and ("or" and)*
and     := unary ("and" unary)*
unary   := "not" unary | atom
atom    := STRING | REGEX | "line" REGEX | "(" expr ")"
STRING  := '"' ( any char but '"' or '\' | '\"' | '\\' )* '"'      (non-empty)
REGEX   := '/' ( any char but '/' or '\' | '\/' | '\' any-other-char )* '/'   (non-empty)
```

Inside a regex literal `\/` is a slash and every other backslash sequence is passed to the regex engine unchanged
(`\s`, `\x{2800}`, `\\`). Regexes use the `regex` crate's syntax (no look-around, linear time). Any other input is a
parse error that names the byte offset.

## 7. Reading a screen

1. Unknown agent (no catalogue entry): `unknown`, no signal.
2. Evaluate **every** signal of the agent (explain needs all of them; detection could stop early but must give the
   same answer).
3. Among the signals whose condition is true, the one with the highest `rank` decides. Equal ranks: the one
   declared first in the file.
4. No signal true: `idle` ("default idle"). An agent that shows nothing we recognise is assumed to be waiting.

The result carries the reading and the deciding signal's name.

## 8. Which agent is in the foreground

### 8.1 Names

`lookup(name)`:

1. Trim, lower-case, take the last non-empty part after a `/`.
2. Drop one trailing `.js`, `.exe`, `.cmd`, `.bat` or `.ps1` (launchers and shims keep them).
3. Match against every agent's `agent` id and `names`. If none matches and an agent has `versioned`, the name
   matches when it is `<versioned>` followed by a digit and anything.

`lookup` is used for process names, for `pane.report_agent` / `agent.start` labels and for plugin labels.

### 8.2 The foreground job

`platform::foreground_job(shell_pid)` returns the terminal's foreground process group and every process in it
(pid, short name, argv). Linux reads `/proc`; macOS uses `libproc`/`sysctl` (§12). Any failure returns `None`.

### 8.3 Identifying one process

For a process with `argv` (and short name `comm`), the **program** is `argv[0]` (or `comm` if argv is empty):

1. **Runtime or shell**: if the program's base name is `node`, `bun`, `python`, `python3`, `pythonX.Y`, `sh`,
   `bash`, `zsh` or `fish`, look for the **script operand**: walk `argv[1..]`; `--` makes the next argument the
   operand; an inline-code flag ends the search with no operand (`-e`, `--eval`, `-p`, `--print` for node and bun;
   `-c` and `-m` for python; `-c` for shells; also their attached forms `-eCODE`, `--eval=CODE`); other options are
   skipped, together with the following argument for options that take one (`-r`, `--require`, `--import`,
   `--loader`, `--experimental-loader`, `--inspect-port`, `-W`, `-X`, `-S`, `-L`, `-o`); the first non-option is the
   operand. The process is the agent the operand **names** (below), or no agent.
2. **Direct**: otherwise, the agent the program names.
3. **Renamed runtime**: otherwise, when `comm` is one of the runtimes of step 1 (a wrapper ran
   `exec -a <name> node script.js`, as Cursor's does), step 1 on `argv`.
4. With no argv at all, `comm` stands in for the program.

A path **names** an agent when (a) `lookup` of its base name finds one, else (b) its path ends with one of an
agent's `scripts` (compared per path component, ignoring case and a final `.js`/`.mjs`/`.cjs`; `*` matches any one
component), else (c) it is a symlink (relative paths with a directory part only) whose target's base name `lookup`
finds.

A process found to be an agent is dropped again when the agent declares batch use and the process is one-shot:
its arguments after the program (or after the script operand) include a `batch_flags` option (compared on the part
before `=`), or `subcommand_is_batch` is set and, after skipping `value_flags` options with their values (`--x v`
or `--x=v`), the next argument exists and does not start with `-` (a subcommand).

### 8.4 Choosing among the job's processes

1. The process group leader (pid = pgid), if it is an agent, decides.
2. Otherwise the agent of the lowest pid among the job's agent processes (normally the parent of the others).
3. None: no agent.

## 9. Local overrides

`$XDG_CONFIG_HOME/tmuxedo/agents/<agent>.toml` (default `~/.config/tmuxedo/agents/`) replaces the bundled file for
that agent, if it is valid and its `agent` equals the file name. Otherwise the bundled file is used and the agent's
catalogue row carries a warning saying why. The catalogue is read once per process; `server.agent_manifests`
reports what was loaded.

## 10. Explain

`explain(agent, input)` returns every signal with its result. JSON (socket `agent.explain`,
`tmuxedo agent explain`):

```json
{
  "agent": "claude",
  "status": "working",
  "signal": "turn",              // the deciding signal, or null
  "default": false,              // true when no signal matched and the agent defaulted to idle
  "source": "built-in",          // or the override path
  "revision": "2026-10-05.1",
  "warning": null,
  "signals": [
    { "name": "turn", "status": "working", "rank": 90, "scope": "last 12",
      "when": "line /…/ or line /…/", "matched": true,
      "scope_bytes": 412, "scope_preview": "…first 240 characters…" }
  ]
}
```

An unknown agent gives `{"agent": "<label or null>", "status": "unknown", "signal": null, "default": false, …}` with
an empty `signals`. The runner adds its own `tmuxedo` object (effective status, source, reported, screen,
heuristic, process kind) and the deprecated `aimux` copy of it.

`server.agent_manifests` returns one row per agent: `{agent, title, source, revision, signals, warning}`.

## 11. How the runner uses it

### 11.1 The detection tick

Every 250 ms, for every live pane: read the foreground job, identify the agent (§8), and, when the agent is known
and the pane produced output since the last reading, read the screen (§7):

- `working` / `blocked` / `idle` set the pane's screen status;
- `unknown` clears it;
- `hold` leaves it as it was;
- **while the program has a synchronized-output frame open (`CSI ? 2026 h` without the closing `l`) the screen is
  not read**: a half-drawn frame can show neither the old nor the new state. The reading happens on the first
  tick after the frame closes (the emulator's sync gate already caps a frame at 100 ms for snapshots).

The alternate screen needs nothing special: `detection_text()` is the active screen, so a full-screen agent UI is
read while it is up and the shell's screen after it exits.

### 11.2 Arbitration (tmuxedo's own, unchanged by this work)

`reported > screen > heuristic`:

- **reported**: a hook (`tmuxedo-runner hook`) or `pane.report_agent` set the status. It wins until it is replaced,
  or cleared when the agent's process leaves the foreground, the pane exits or the session ends (stale).
- **screen**: §7, for the identified (or reported) agent kind.
- **heuristic**: output in the last 1.5 s → working; a bell or OSC 9/777 notification after work → `done`; else
  idle.

A change of effective status emits `pane.agent_status_changed`, the durable `agent.*` events, the protocol `agent`
frame (web, app, TUI) and, for `blocked` and `done`, a push.

## 12. Platforms

| | Linux | macOS |
|---|---|---|
| foreground pgid of a shell | `/proc/<pid>/stat` field 8 (`tpgid`) | `proc_pidinfo(PROC_PIDTBSDINFO).e_tpgid` |
| processes of a group | scan `/proc/*/stat` field 5 (`pgrp`) | `proc_listpids(PROC_PGRP_ONLY, pgid)` |
| short name | `/proc/<pid>/stat` `(comm)` | `pbi_name` (or `pbi_comm`) |
| argv | `/proc/<pid>/cmdline` | `sysctl(KERN_PROCARGS2)` |
| cwd | `/proc/<pid>/cwd` | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` |
| tty | `/proc/<pid>/fd/0` | `devname(e_tdev)` → `/dev/ttysNNN` |

Other platforms compile and return `None` everywhere (identification then relies on hooks and `agent.start`).

## 13. Supported agents and their signals

Ranks only order the signals of one agent file; what matters is the order between signals of *different*
status that can be true together.

Text in quotes is the UI text the agent shows (case is ignored for quoted text unless the signal uses a regex).

### claude — Claude Code (`claude`, also `claude-code`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-busy | working | title | title starts with a braille spinner glyph or a half-circle glyph (◐◑◒◓), then a space |
| transcript | hold | last 3 | "showing detailed transcript" with a viewer hint (ctrl+o … to toggle, ctrl+e … show all / collapse, ↑↓ scroll, ? for shortcuts) |
| form | blocked | below-rule | "esc to cancel" with "enter to confirm", or "enter to select" plus a navigation hint |
| workflow-question | blocked | screen | "run a dynamic workflow?" and "esc to cancel" |
| mcp-input | blocked | screen | an "MCP server "…" requests your input" header line, an Accept or Decline option line, and "esc to cancel" |
| btw | working | last 5 | a `/btw` line and an "esc to close" line (the side-question overlay while a turn runs) |
| turn | working | last 12 | the mode line with "esc to interrupt", or a status line: a sparkle glyph, text ending in "…", optionally an elapsed timer |
| background-agents | working | above-box-line | "Waiting for N background agent(s) to finish" status line |
| mcp-tasks | working | last 12 | a column-0 activity line ending "· N MCP task(s) still running" (may wrap), not while a permission prompt is up |
| prompt-box | idle | box | the `❯` input line in the prompt box, with no picker hint in the box |
| model-picker | hold | screen | "select model", "enter to set as default", "esc to cancel", and not a permission prompt |
| bash-approval | blocked | screen | "do you want to proceed?" with a Bash cue and a Yes/No option line (the cursor `❯` may precede it) |
| approval | blocked | below-rule | "do you want to proceed?", "esc to cancel" and a numbered Yes/No option |
| question-text | blocked | screen | question wording ("do you want to …", "would you like to …" with a yes or `❯`, "waiting for permission", review/interview prompts…), unless the composer line `❯` is empty and live |
| title-idle | idle | title | title starts with "✳ " |
| progress-idle | idle | progress | progress starts with `4;0` |

### codex — Codex CLI (`codex`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-action | blocked | title | "Action Required" |
| title-busy | working | title | a braille spinner frame as a word |
| transcript | hold | after-composer | the pager footer ("↑/↓ to scroll", "pgup/pgdn to", "home/end to jump", "q to quit") with "esc to edit prev" |
| trust | blocked | first 20 | the folder-trust screen (first line "> You are in …" or "Folder access", and the trust question) |
| update | blocked | last 20 | the startup update offer ("Update available!", "Update now", "Skip until next version", ends with "Press enter to continue") |
| approval | blocked | after-composer | a live approval or question footer ("press enter to confirm or esc to cancel", "enter to submit answer/all", "allow command?", the file-search picker) |
| question-text | blocked | unless-composer | "[y/n]", "yes (y)", "do you want to …" / "would you like to …" with a yes or `❯`, unless a composer line with a sparkle is the live one |
| turn | working | before-composer | a status line with an elapsed timer "(…s)" (optionally "• … to interrupt"), with nothing but queued-input lines after it, and not "Reconnect failed" |
| title-idle | idle | title | any title that is not busy or "Action Required" |

### gemini — Gemini CLI (`gemini`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| approval | blocked | screen | "│ Apply this change", "│ Allow execution", a proceed/confirmation question with "yes", or a `❯` option with yes/allow |
| turn | working | screen | "esc to cancel" |

### cursor — Cursor CLI (`cursor-agent`, also `cursor`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| write-approval | blocked | last 8 | "write to this file?" + "proceed (y)" + a reject/skip choice |
| approval | blocked | screen | the run/allow approval footers ("run this command?" with run/skip keys, "(y) (enter)", "allow … (y)", "keep (n)", "skip (esc or n)", "run … (y)") |
| turn | working | last 6 | "ctrl+c to stop" |
| background | working | last 5 | "N background task(s)" |
| spinner | working | last 8 | a hexagon or braille spinner followed by a word ending in "ing" |

Process: also the Cursor install layout `cursor-agent/versions/<v>/index.js` run by node.

### devin — Devin CLI (`devin`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| trust | blocked | last 8 | the workspace-trust question |
| approval | blocked | last 8 | "approve once", "select", "confirm", "esc cancel" |
| tools | working | last 8 | "running tools" + "esc to interrupt", no approval footer |
| guide | working | last 6 | "guide devin while it works", no approval footer |
| shell-read | working | last 8 | "reading shell " + "timeout:", no approval footer |
| welcome | idle | last 8 | the welcome prompt "❭ Ask Devin to build features, fix bugs…", nothing working or blocking |
| prompt | idle | last 6 | a `❭` prompt line with "context:", nothing working or blocking |

### agy — Antigravity CLI (`agy`, also `antigravity`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| approval | blocked | screen | "requesting permission for:" with "do you want to proceed?" or the amend/edit hint |
| spinner | working | screen | a braille spinner then an "…ing" word |
| background | working | last 5 | "· N task(s)" |

### cline — Cline CLI (`cline`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| tool-approval | blocked | screen | "let cline use this tool", or an act/plan-mode "execute command?"/"use this tool?" with yes |
| inline-approval | blocked | last 16 | "Cline needs permission", "Approve tool call?", "[y] Approve", "[n] Deny" |
| question | blocked | last 24 | "Cline is asking a question", an answer line "> …", "(Tab)" and "Shift+Tab" |
| turn | working | last 20 | a braille spinner line, or "Thinking... (esc to cancel)" |
| composer | idle | last 12 | the `❯` composer with the rule and the Tab/Shift+Tab footer |
| default-working | working | screen | any non-empty screen (Cline shows no reliable idle chrome otherwise) |

### opencode — opencode (`opencode`, also `opencode2`, `open-code`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| permission | blocked | screen | "△ Permission required", or the dialog footer ("esc dismiss" + enter confirm/submit/toggle + a select/tab hint) |
| interrupt | working | screen | "esc to interrupt", "ctrl+c to interrupt", or an opencode line with "esc (again to) interrupt" |
| progress-bar | working | screen | four or more ■/⬝ blocks |

### kilo — Kilo Code (`kilo`, also `kilo-code`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| permission | blocked | screen | as opencode's permission |
| interrupt | working | screen | "esc interrupt" |

### copilot — GitHub Copilot CLI (`copilot`, also `github-copilot`, `ghcs`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| approval | blocked | screen | the permission or confirmation dialog (see the agent file) |
| turn | working | screen | the cancel hint while a turn runs |

### kimi — Kimi Code (`kimi`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| approval | blocked | screen | "↵ confirm" with a run/write/apply/stop/plan question, the choose hint and approve/reject/revise |
| question | blocked | screen | a "question" panel with a `? ` line, "↑↓ select", "esc cancel" and a choose/toggle/save hint |
| approval-v1 | blocked | screen | the older "requesting approval" panel with approve once / for this session |
| agents | working | last 3 | "kimi… thinking … [N agent(s) running]" |
| moon | working | screen | a moon-phase spinner alone on a line |
| spinner | working | screen | a braille spinner with "thinking...", "working..." or "using " |

Process: also `node_modules/@moonshot-ai/kimi-code/dist/main.mjs`.

### kiro — Kiro CLI (`kiro-cli`, also `kiro`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| prompt | idle | last 4 | the "ask a question or describe a task" composer line |
| tool-approval | blocked | last 8 | the approval menu footer ("esc to close" with navigate/select hints) and one of Kiro's option menus shown whole, the cursor on one option (a menu cut by a wrap is not trusted) |
| tool-approval-edit | blocked | screen | the "requires approval · modify request" panel |
| crew-approval | blocked | last 8 | the sub-agent "tool approval" menu |
| question | blocked | last 8 | "to navigate", "to submit", "esc to cancel" |
| turn | working | last 4 | "kiro is working", "type to steer", "ctrl+s to queue" |
| title-busy | working | title | a spinner glyph then "kiro:" |
| progress-busy | working | progress | `4;3` |

### droid — Factory Droid (`droid`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| execute-approval | blocked | screen | "enter to select", "esc to cancel", a navigate hint and a "> yes, allow" / "> no, cancel" option |
| menu | blocked | last 8 | "enter select", "esc cancel" and a navigate hint |
| stop | working | screen | "esc to stop" |

### amp — Amp (`amp`, also `amp-local`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-plugin | blocked | title | "Plugin confirmation needed" |
| title-busy | working | title | starts with a braille spinner |
| approval | blocked | screen | an approval footer or tool confirmation |
| footer | working | last 5 | "╰ … thinking/streaming/running tools/waiting ─" |
| cancel | working | screen | "esc to cancel" |
| title-idle | idle | title | " - amp - " and not busy or confirming |

### grok — Grok CLI (`grok`, also `grok-build`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-action | blocked | title | "Action Required" |
| options | blocked | screen | an option row "┃ x (●/○) …" |
| permission-hints | blocked | last 2 | ":select", "ctrl+o:yolo", "ctrl+c:cancel" |
| question-hints | blocked | last 2 | "tab:scrollback", "shift+x:dismiss" |
| scope-picker | blocked | screen | "yes, proceed", "no, reject" and the scope hint |
| chip | working | first 1 | the background-work chip "· N │" |
| background | working | last 12 | "○ N commands/monitors/loops/subagents still running" |
| spinner | working | screen | a braille spinner line ending "[stop]" |
| cancel-hints | working | last 2 | "ctrl+.:shortcuts" with esc/ctrl+c cancel |
| progress-busy | working | progress | `4;1;-1` |
| tool | working | screen | a waiting tool footer, or a spinner + Run/Read/Search/List |
| title-idle | idle | title | ends with "grok", no spinner |
| title-busy | working | title | a braille spinner as a word |
| progress-idle | idle | progress | `4;0;0` |
| hints-idle | idle | last 2 | "ctrl+.:shortcuts" without a cancel hint |

### hermes — Hermes Agent (`hermes`, also `hermes-agent`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-alert | blocked | title | starts with ⚠ |
| title-busy | working | title | starts with ⏳ |
| interrupt | working | last 5 | "msg=interrupt" or "ctrl+c to interrupt" |
| command-approval | blocked | last 14 | a dangerous-command / approval menu with a confirm/select hint |
| clarify | blocked | last 14 | a clarification question with a confirm/send/select hint |
| credentials | blocked | last 14 | sudo password, skill setup, a 🔑 prompt |
| confirm | blocked | last 14 | approve once/cancel or new-session/keep-going with a confirm hint |
| cancel | working | last 5 | "ctrl+c cancel" |
| title-idle | idle | title | starts with ✓ |

Process: python-wrapped `hermes` scripts.

### letta — Letta Code (`letta`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| progress-alert | blocked | progress | `4;3` |
| title-action | blocked | title | "[ ! ] Action Required" / "[ . ] Action Required" |
| command-approval | blocked | last 20 | "Run this command?" and "Enter to select · Esc to cancel" |
| title-busy | working | title | a braille spinner as a word |
| status | working | last 8 | "… is …… (esc to interrupt …)" or "(interrupting)" |
| tool | working | last 8 | "Running..." |
| profiles | unknown | last 12 | the agent profile selector |
| typing | unknown | last 8 | text typed in the `›` composer (not the "Try …" hint) |
| composer | idle | last 8 | an empty `›` composer or the "Try …" hint, nothing running |
| no-evidence | unknown | screen | always (Letta never defaults to idle) |

Process: one-shot runs (`-p`, `--json`, subcommands, …) are not the interactive agent.

### maki — Maki (`maki`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| permission | blocked | screen | "permission required" with allow/deny keys |
| plan | blocked | screen | "plan complete" + "enter confirm" with a plan action |
| status-busy | working | last 1 | a braille spinner before the [BUILD]/[PLAN]/[BASH] status bar |
| status-idle | idle | last 1 | the status bar without a spinner |
| prompt | idle | last 3 | a `❯ ` prompt, not queueing and no spinner |

### muse — Muse (`muse`, also `muse-code`, `muse-cli`, `muse-bin-<version>`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| trust | blocked | last 12 | "Do you trust this workspace?" with its choices |
| pick | blocked | last 8 | a pick/toggle request footer |
| menu | hold | last 8 | an overlay menu (confirm/save/go back, toggle/close/filter) |
| turn | working | last 8 | "esc to interrupt", not a pick request |
| approval | blocked | last 8 | allow once / session / proceed choices |
| prompt | idle | last 5 | the `⟩` prompt, nothing running, no overlay |
| status-idle | idle | last 3 | the model · effort · status line, nothing running |

### qodercli — Qoder CLI (`qodercli`, also `qoder`, `qodercn`, `qoderclicn`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| question | blocked | screen | waiting/awaiting confirmation with a choice, permission required, "allow once or always?", "asking user", "enter your response", "review your answers:", "shell awaiting input" |
| cancel | working | screen | "(esc to cancel," |
| spinner | working | screen | a braille spinner then a word |

### qwen — Qwen Code (`qwen`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| title-alert | blocked | title | starts with ✳ |
| title-busy | working | title | starts with ◐ |
| confirming | blocked | last 20 | the "waiting for user confirmation…" spinner line (in any of its UI languages) |
| tool-approval | blocked | last 20 | "yes, allow once" with an apply/execute/proceed question |
| question | blocked | last 20 | a numbered `❯` option and the "↑/↓ : … Enter :" hint |
| trust | blocked | last 20 | the folder-trust dialog |
| turn | working | last 8 | a spinner line with a timer and "· esc to cancel)" |
| turn-narrow | working | last 8 | the timer and "· esc to cancel)" alone (narrow pane) |
| progress-busy | working | progress | `4;3` |
| composer | idle | last 30 | the `>` composer with its "type your message" placeholder (also when wrapped) |

Process: also `node_modules/@qwen-code/qwen-code/dist/index.js`.

### pi — Pi (`pi`)

| Signal | Status | Scope | Evidence |
|---|---|---|---|
| working | working | screen | "Working...", or a braille spinner + "Working" line |
| border | working | last 12 | the "── ⠋ Working ───" border |

Process: also the `@earendil-works/pi-coding-agent` CLI script.

### omp — Oh My Pi (`omp`), mastracode — Mastra Code (`mastracode`)

No screen signals: identified by process (including their package scripts) and driven by hooks / `report_agent`;
the screen reads `idle`.

## 14. Tests (the bar)

- **100 % line and branch coverage** of `runner/crates/detect`, measured with `cargo llvm-cov --branch` (nightly)
  in the Rust container and gated in CI (private `runner.yml`, public `ci.yml`).
- One test per agent file and per status it can produce, plus a transition sequence per agent
  (idle → working → blocked → working → idle) through the emulator.
- Scopes, the condition language (parser and evaluator) and validation, each case and each error.
- Edge cases: partial frames (every byte prefix of each recording), synchronized output, alternate screen, wide
  characters, resize, stale status, hook-reported versus screen precedence.
- Property tests (proptest): the parser never panics and round-trips; evaluation of `not`/`and`/`or` obeys boolean
  algebra; scopes are always slices of the input; detection never panics on arbitrary text.
- `tmuxedo agent explain` output, `server.agent_manifests`, `pane.process_info`.
- Parity (§15) against the previous engine before it was removed.

## 15. Parity with the previous engine

Measured on 2026-10-05 with a temporary differential harness that linked the previous engine and this one side by
side, before the previous engine was deleted. Every case was read by both engines for all 24 agents.

| Run | Screens: fixed corpus | Screens: random and mutated | Differences |
|---|---|---|---|
| 2 | 41,439 (every byte prefix of the six recordings at 40×120, 24×80, 50×200, 12×40, plus 105 screen files) | 20,000 | 0 |
| 3 | 41,504 | 100,000 | 1, a bug in this engine's Kiro file (an approval menu cut by a wrap was accepted), fixed |
| 4 (final; re-recorded htop/nvim and scrubbed claude.bin) | 32,303 (32,132 recording frames + 171 screen files) | 50,000 | **0** |

The fixed corpus is every recorded screen in the repository (`runner/tests/fixtures/*.bin`, the Claude plugin's test
screens) and this crate's stand-in screens. Random screens are built from both engines' text fragments plus box
drawing, composer lines, spinners and timers; mutations wrap lines, re-indent, swap glyphs, change digits, truncate,
drop, duplicate and reorder lines. The previous engine's readings of every byte prefix of the recordings are kept as
`runner/crates/detect/tests/golden/recordings.json` and checked by `tests/emulated.rs`.

Processes (run 4, 99,587 single-process and whole-job cases): no case where the previous engine found an agent and
this one found none or another. All differences are this engine finding an agent the previous one missed, or the
choice rule for jobs with several different agents:

| Class | Cases | Why (intentional) |
|---|---|---|
| a runtime started under another name (`exec -a agent node …/index.js`): comm is the runtime, argv[0] is not | most of the 2,019 "found" cases | §8.3 step 3; this is how Cursor's installer starts it |
| node running `cursor-agent/versions/<v>/index.js` | part of the 483 Cursor cases | the Linux/macOS install layout (§13 cursor); the previous engine knew only the Windows layout |
| Letta wrapped by python | part of the 444 Letta cases | the previous engine called every python-wrapped Letta a one-shot run |
| jobs with two or three different agents and no agent leader | 1,799 | lowest pid wins (§8.4) instead of a confidence score |

### 15.1 Intentional differences (all of them)

| Area | Previous | Now |
|---|---|---|
| Screen readings | — | identical on every case above |
| Visible-evidence flags (`visible_idle`, `visible_blocker`, `visible_working`) | reported, unused by tmuxedo | dropped |
| Viewers | `state = unknown` + `skip_state_update` | a `hold` reading (same effect: the last status is kept) |
| Synchronized output | read mid-frame | not read while a frame is open (§11.1) |
| Explain / `server.agent_manifests` JSON | rule-and-region shape, remote-catalogue fields | §10 shapes; `runner/api/runner-api.schema.json` |
| Remote manifest updates | a downloader existed (never enabled) | none |
| Local overrides | `~/.config/<app>/agent-detection/<agent>.toml`, previous format | `~/.config/tmuxedo/agents/<agent>.toml`, tmuxedo's format; anything else is ignored with a warning |
| Windows-only argv parsing (`cmd /c`, PowerShell `-File`, `node.exe` Cursor) | present | removed (the runner runs on Linux and macOS); `.exe`/`.cmd`/`.bat`/`.ps1` endings are still stripped from names |
| Agent label aliases naming another program's integrations | present | removed |
| Process identification | see the table above | §8 |

