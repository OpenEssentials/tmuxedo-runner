# tmuxedo relay protocol v1

The contract between **runner** (Rust, on a machine), **gateway** (Node, server side) and **clients** (web, PWA, `tmuxedo` TUI/CLI). Golden fixtures in `packages/protocol/fixtures/` are tested from TypeScript and Rust; a change here changes both or neither.

## Principles

- **The runner owns the truth** (topology, pane bytes, agent state). The gateway routes and remembers nothing durable; Postgres holds a projection that web writes.
- **The gateway never parses terminal bytes.** Data frames are opaque payloads behind a small fixed header, so end-to-end encryption can be added later without changing the routing.
- **Offsets, not acknowledgements of delivery.** Every pane is an append-only byte stream with a 64-bit offset. Attach, reconnect, gateway restart and slow-client resync are all "send me pane P from offset N".
- **A slow viewer never slows a pane.** Backpressure drops a viewer to resync; it never pauses a PTY.
- **Parsers never throw.** Junk becomes a named refusal or is ignored and counted.
- **Every hop pings at ≤ 25 s.** Cloudflare closes silent sockets at about two minutes (decision 0001).

## Sockets

Both use WebSocket subprotocol `tmuxedo.v1`. Clients offer `tmuxedo.v1, aimux.v1` (plus the ticket) and accept whichever the gateway selects; the gateway selects `tmuxedo.v1` when offered, else the pre-rename `aimux.v1`, which stays accepted indefinitely.

| Path | Who | Authentication |
|---|---|---|
| `/gw/runner` | runner | `Authorization: Bearer tmr.<lookup>.<secret>` header (tokens minted before the rename carry `amr.` and stay valid). The gateway asks web (`POST /api/internal/runner-auth`, bearer `GATEWAY_RPC_SECRET`) and caches a positive answer for 60 s. Identity (machine, user) comes from the token, never from frames. |
| `/gw/client` | browser, PWA, TUI | Attach ticket in `Sec-WebSocket-Protocol: tmuxedo.v1, aimux.v1, ticket.<ticket>`; browser `Origin` must be allowed. Ticket `mid` binds the socket to one machine for pane data; without `mid` the socket gets machine list and events only. Ticket `scope` is `observe` or `control`. |

A second runner socket for the same machine replaces the first (the old one is closed with 4001 `replaced`).

## Text frames (JSON, one object per frame, field `t` is the type)

Unknown `t` values are ignored (forward compatibility). Every frame may carry `id` for request/response pairing.

### runner → gateway

| `t` | Fields | Meaning |
|---|---|---|
| `hello` | `v` (=1), `runner` {`version`, `holderVersion`, `os`, `arch`, `hostname`, `claudeVersion`?, `claudeAuth`?, `features`?} | First frame. Gateway answers `welcome` or closes 4002 `unsupported_version`. `features`: protocol additions this runner serves (`["glance"]`, see "Glances"); absent on older runners. `claudeAuth` = {`state`: `ok`\|`missing`\|`expired`\|`failed`, `reason`, `since`}, absent when the machine has no Claude Code; changes after `hello` arrive as `auth_*` events (see "Claude auth health"). |
| `topology` | `rev`, `workspaces`[], `tabs`[], `panes`[], `focusedWorkspace`? | Full topology, sent after `hello` and on every change. Shapes below. Focus (`focusedWorkspace`, each workspace's `activeTab`, each tab's `focusedPane`) is per machine and shared by every client; `pane.focus` / `pane.focus_direction` / `tab.focus` / `workspace.focus` change it. |
| `status` | `paneId`, `agent` (Agent, or null) | Agent state change for one pane, including a change to its helpers (`agent.children`, "Helpers" below). |
| `event` | `seq`, `paneId`, `type`, `at`, `data` | Durable agent event (`blocked`, `done`, `prompt_expired`, `auth_ok`, `auth_missing`, `auth_expired`, `auth_failed`, `memory_stopped`, `resumed`, ...). `seq` is per machine and monotonic; web stores `(machineId, seq)` uniquely, so replays are harmless. `paneId` is `""` for machine-level events. Unknown `type`s must be stored and otherwise ignored. |
| `res` | `id`, `result` \| `error` {`code`, `message`} | Response to a `req`. |
| `size` | `paneId`, `cols`, `rows` | The PTY's current size (after a resize). |
| `ping` / `pong` | `at` | Heartbeat. |

**Open-URL requests (`event` type `open_url`).** Panes get `BROWSER` and PATH stand-ins for `xdg-open`, `open`, `sensible-browser`, `x-www-browser` and `www-browser` that call the runner. The runner validates the URL (http/https only, length ≤ 8 KiB) and emits `event {type:"open_url", paneId, data:{url, source:"browser-shim", program}}`. These are NOT durable (no seq persistence, not stored by web, not pushed): the gateway forwards them only to clients currently attached to that machine. Web shows a toast with the URL's host and an Open button (user gesture); the native app opens it in its in-app browser; the TUI opens it with the local OS opener. The stand-in exits 0 immediately so the calling program (e.g. Claude's /login) continues with its own fallback. Claude Code 2.1.x on Linux runs `$BROWSER <url>` when `BROWSER` is set, else `xdg-open`, and gives up (`no_display`) when neither `BROWSER` nor a display is set; panes always have `BROWSER` (the `x-www-browser` stand-in), so `/login` reaches the user's device. `tmuxedo attach` prints the link as a clickable OSC 8 line, or opens it locally with `--open-urls`. When the URL is an OAuth authorize URL whose `redirect_uri` is the machine's own loopback, `data` also carries `callback` {`host`, `port`, `path`, `state`} (see "Login callback relay").

### gateway → runner

| `t` | Fields | Meaning |
|---|---|---|
| `welcome` | `machineId`, `userId`, `lastEventSeq` | Admission done. `lastEventSeq` is the highest event `seq` web has stored for this machine (0 if none). The runner MUST number its next event above `max(local seq, lastEventSeq)`, and renumber any unacknowledged queued events that are ≤ `lastEventSeq` before sending them. This makes a runner that lost or discarded its state (reinstall, upgrade, wiped volume) unable to collide with stored events. |
| `attach` | `req` (u32), `paneId`, `from`? (u64 as number) | Start (or restart) a stream for request `req`: if `from` is inside the ring, replay from it as `OUTPUT` frames tagged `req`; otherwise send one `SNAPSHOT` tagged `req`. Then the pane is live: live `OUTPUT` frames carry `req` 0 (broadcast). |
| `detach` | `paneId` | No viewers left; the runner stops sending live bytes for it. |
| `req` | `id`, `method`, `params` | A runner API method (see "Methods"), forwarded from a client, an API token holder, or web. |
| `resize` | `paneId`, `cols`, `rows` | From the pane's size owner only. |
| `decision` | `paneId`, `promptId`, `behavior` (`allow`\|`deny`), `message`? | Answer to a held permission prompt. Stale `promptId` → the runner replies with an `event` of type `decision_stale`. |
| `login_callback` | `id`, `paneId`, `url` | Deliver the address the user's browser ended on to the pane's pending login (see "Login callback relay"). Answered with `res {id}`. Sent only to runners whose `hello.runner.features` contains `"login_callback"`. |
| `events_ack` | `upTo` (seq) | Every `event` with `seq ≤ upTo` is durably stored by web. Sent only after web's `agent-events` call succeeds. The runner keeps unacknowledged events in an on-disk queue (survives runner crash and holder restart) and resends them after every `welcome`; web ignores duplicates by `(machineId, seq)`. The gateway never acks past a seq still on its way to web or not stored (refused, or dropped from its bounded queue), and resets its ack state on each `welcome`, so a resent event is acked again. A seq web **rejects** (it already holds a different event under it) is not stored but no longer holds acks back: the gateway sends `events_rejected` first, then acks past it. |
| `events_rejected` | `seqs`[], `lastEventSeq` | Web refused these seqs because it already holds different events under them (the runner reused numbers). The runner renumbers those queued events above `max(local seq, lastEventSeq)`, in order, and resends them, **before** it applies any later `events_ack` (which may cover the old numbers). Sent before that ack on the same socket. |
| `config` | `claude` {`args`: string[]}, `shell` {`aliases`: {name: command}, `env`: {NAME: value}, `rc`: string}, `workDir`?: string | Launch and shell configuration from web (the user's Claude launch options merged with any per-machine override), sent after `welcome` and on change. The runner persists it and its PATH `claude` shim prepends `args` (before `--settings` and the user's own arguments) for every launch, resume and `agent.start`. Validated by the runner: each arg must start with `-` (or be the value of the preceding flag), no shell metacharacters are interpreted (exec, not a shell). `shell` applies to every NEW pane: the runner writes `~/.config/tmuxedo/shell-init.sh` (aliases as `alias name='…'` with single-quote escaping, `export NAME='…'` for env, then the `rc` snippet verbatim) and starts bash with an rcfile that sources the user's `~/.bashrc` first and then shell-init.sh (zsh: a ZDOTDIR wrapper whose .zshrc sources the real one first); alias names `[A-Za-z_][A-Za-z0-9_-]*`, env names `[A-Z_][A-Z0-9_]*`, runner-managed variables (`TMUXEDO_*`, `AIMUX_*`, `BROWSER`, `PATH`) cannot be overridden. Existing panes are unchanged until the user runs `tmuxedo reload-shell` (re-sources the file). `workDir` (optional; absent = unchanged, `""` = the default) is the folder NEW panes start in when no cwd is given: `~`, `~/…` or absolute, default `~/work`, created if missing (a workspace anchored at `$HOME` also opens new tabs there; an explicit cwd is always used as given). Not `$HOME` because Claude Code never remembers "trust this folder" for the home directory. After a pod loss a Claude session recorded in `$HOME` itself (also through a symlink, or the pre-rename `/home/aimux`) resumes in `workDir`; its `resumed` event then carries `movedFrom` (the old cwd); sessions in any other folder resume where they were. Web always sends it (Settings → Terminal → "Where new terminals open", and the machine's own folder on its settings page); `""` when nobody chose one. |
| `ping` / `pong` | `at` | |

### client → gateway

| `t` | Fields | Meaning |
|---|---|---|
| `hello` | `client` {`kind`: `web`\|`pwa`\|`tui`\|`cli`, `version`} | Answered with `welcome`. |
| `attach` | `paneId`, `from`? | Watch a pane; `from` is the last offset the client has fully processed. |
| `detach` | `paneId` | |
| `ack` | `paneId`, `offset` | Bytes processed up to `offset` (flow control). Send at least every 64 KiB processed. |
| `resize` | `paneId`, `cols`, `rows` | Becomes the size owner (requires `control`). |
| `req` | `id`, `method`, `params` | Runner API method (mutating methods require `control`). |
| `decision` | `paneId`, `promptId`, `behavior`, `message`? | Requires `control`. |
| `login_callback` | `id`, `paneId`, `url` | Requires `control` on a socket bound to the machine, by its owner (else `res` error `forbidden_scope`). Answered with `res {id}` (see "Login callback relay"). |
| `ping` / `pong` | | |

### gateway → client

| `t` | Fields | Meaning |
|---|---|---|
| `welcome` | `sub`, `mid`, `scope`, `machine` {`online`, `hello`?} | |
| `machine` | `online`, `hello`? | Runner connected or disconnected. On a socket without `mid` these frames (and `status`, `event`) carry `machineId` and cover all of the user's machines; its `welcome` has `mid: null`, `machine: {online: false}`. |
| `topology`, `status`, `event`, `size` | as from the runner | Fan-out of the latest state. |
| `owner` | `paneId`, `you` (bool) | Who owns the pane's size. Non-owners letterbox and may `resize` to take it. |
| `res` | `id`, `result` \| `error` | |
| `error` | `code`, `message` | Refusal of a client frame: `forbidden_scope`, `machine_offline` (the attach is kept and completes when the runner returns), `bad_frame`, `no_machine` (a frame that needs `mid` on a socket without one). |
| `ping` / `pong` | | |

### Shapes

```
Workspace { id, name, cwd, position, activeTab? }
Tab       { id, workspaceId, name, position, layout, focusedPane? }   // layout: split tree, see below
Pane      { id, tabId, cwd, command, title, cols, rows, exited?: { code, at } , agent: Agent | null, label? }   // label: the pane's name, see "Names"
Agent     { kind, status: 'working'|'blocked'|'done'|'idle'|'unknown', since, source: 'reported'|'screen'|'heuristic',
            sessionId?, promptId?, prompt?: { tool, input, suggestions? }, summary? }
layout    = { pane: id } | { dir: 'h'|'v', ratio: number, a: layout, b: layout }
```

Ids are runner-assigned (`w1`, `w1:t1`, `w1:p3`), unique per machine.

## Binary frames (data)

```
offset  size  field
0       1     kind       1 OUTPUT (runner→gw→client)  2 INPUT (client→gw→runner)  3 SNAPSHOT (runner→gw→client)  4 CHAT ("Agent chat")
1       1     flags      bit0 GAP: bytes before this frame were lost (ring wrapped); bit1 EPHEMERAL (CHAT only); others reserved = 0
2       4     req        u32 BE. 0 = live broadcast; otherwise the attach request this answers
6       1     idLen      length of paneId in bytes (1..255)
7       n     paneId     UTF-8
7+n     8     offset     u64 BE. OUTPUT/SNAPSHOT: stream offset of the first payload byte (SNAPSHOT: the offset live output resumes at). INPUT: 0
15+n    …     payload    raw bytes (OUTPUT/INPUT) or ANSI that recreates scrollback + screen + modes (SNAPSHOT)
```

Binary frames are at most 1 MiB in both directions, and so are client text frames. A runner's text frames may be up to **16 MiB** (its socket's limit; a larger frame closes it with 1009): a `res` above **4 MiB** is never forwarded; its caller gets `res` `error.code: "too_large"` and the runner socket stays up. The runner keeps every reply under **1 MiB** itself, so it is safe with any gateway: `pane.read` / `agent.read` return at most 10 000 lines and 512 KiB of JSON-encoded text, keeping the newest part (cut at a line start) with `truncated: true`; any other reply that would exceed 1 MiB is sent as `error.code: "too_large"` instead. The runner coalesces output (≈10 ms or 32 KiB) and round-robins panes so one flooding pane cannot starve the rest.

**Gateway routing:** the gateway rewrites nothing but `req` (it maps its per-runner `req` numbers back to the client that asked). `req` 0 output goes to every client attached to the pane and not in resync.

### Pane history (snapshots and older lines)

Each pane keeps a long **history**: the lines that scrolled off the top of its main screen, by default
**50,000 lines** (20,000 on a hosted machine), at most 32 MiB (hosted 8 MiB) per pane. A machine sets
its own with `history_lines` / `history_mib` in `runner.toml` or `TMUXEDO_HISTORY_LINES` /
`TMUXEDO_HISTORY_MIB` (decision 0009). Every history line has an **index** that only grows; `CSI 3 J`
(what `clear` sends) and `ESC c` empty the history and indices keep counting. Lines that a program
scrolls off again while redrawing the same frame inside a synchronized update (`CSI ? 2026 h … l`)
are kept once.

A `SNAPSHOT` from a runner that keeps history starts with `ESC c` and then

```
ESC ] 777 ; tmuxedo-history ; start=<S> ; first=<F> ; end=<E> ; limit=<L> BEL
```

before the scrollback it writes: the snapshot's scrollback is history lines `F … E-1` (the newest that
fit in about 96 KiB, so a phone opens quickly), `S` is the oldest line the runner still has, `L` its
line limit. Terminals that do not know the OSC ignore it (xterm.js, `vt100`). A viewer that knows it
pages back from `F` with the runner method **`pane.read`** `{pane_id, source: "history", before?,
lines?, format?}` (a read: allowed to watchers and in observe-only mode):

- `before`: the page ends right before this index (default: the newest line); `lines`: at most this many
  (default 2,000, at most 10,000), and at most 512 KiB of JSON-encoded text;
- `format` `ansi` (default): `text` is each line as the ANSI that draws it from column 0, followed by
  CRLF, ready to write into a terminal; `text`: plain lines joined by `\n`;
- the result's `read.history` is `{first, count, start, end, limit, more}`: the page holds lines
  `first … first+count-1`; `more` says older lines exist. A `before` older than `start` gives an empty
  page with `more: false`.

A snapshot without the OSC (an older runner) means "no history paging": the viewer keeps what it has.

## Flow control (gateway, per client per pane)

- `sent` = offset after the last forwarded byte; `acked` = the client's last `ack`.
- If `sent − acked` exceeds **1 MiB**, stop forwarding live output to that client for that pane (state `behind`).
- When the client's `ack` catches up, the gateway sends the runner `attach {req, paneId, from: acked}`; the runner answers with replay or a snapshot, and the client is live again.
- WebSocket `bufferedAmount` above 4 MiB on either hop also triggers `behind` for that client (or, on the runner socket, the runner coalesces harder).

## Size ownership

The last client to send `resize` (or to type, if it has never resized) owns the pane size. Everyone else gets `owner {you: false}` and renders at the PTY size (`size` frames). Phones attach as observers and never resize implicitly.

## Methods (`req`)

Names, parameters and results are tmuxedo's runner API, specified by its JSON Schema (`runner/api/runner-api.schema.json`) and checked against it by the runner's conformance tests. The runner serves the identical set on its local unix socket (`$TMUXEDO_SOCKET_PATH`, also exported as the deprecated `AIMUX_SOCKET_PATH`) as newline-delimited JSON `{"id","method","params"}` → `{"id","result"|"error"}`. A method the runner does not have returns `{"error":{"code":"unknown_method"}}`; one it serves on the local socket only returns `not_supported` over the gateway.

## Client settings (web API, read by the `tmuxedo` TUI)

`GET /api/me/keybindings` (session or PAT with `read`; MFA-gated like every `/api/me` route) returns the user's effective keybindings. The web machine page uses the same document; `PUT` (session only) replaces them, `DELETE` resets to the defaults. Code: `web/src/lib/keybindings.ts`.

```json
{
  "version": 1,
  "prefix": "ctrl+b",
  "bindings": {
    "new_tab": "c", "split_right": "%", "split_down": "\"",
    "focus_left": "left", "focus_right": "right", "focus_up": "up", "focus_down": "down",
    "zoom": "z", "close_pane": "x", "detach": "d", "toggle_full_window": "f",
    "rename_tab": ",", "rename_workspace": "$", "rename_pane": "P",
    "arrange": "space", "even_out": "E"
  },
  "isDefault": true,
  "overlay": "on"
}
```

- **Prefix overlay** (`overlay`, queue 3j): `"on"` (default; after the prefix a panel lists every prefix action with its key, greyed out with the reason when it does not apply now, and a key or a click runs it), `"delay"` (shown only when no key follows within 700 ms), `"off"`. A client that does not know the field ignores it; an absent or unknown value means `"on"`. Change it with `PUT /api/me/prefix-overlay {mode}` (a browser session, or a token with `write`, which is how the TUI's "don't show this again" works; it is a display preference, audited as `settings.prefix_overlay`); `GET /api/me/prefix-overlay` → `{mode}`. Web: `web/src/lib/prefix-overlay.ts`; TUI: `runner/crates/runner/src/cli/tui/overlay.rs`.
- **How "+ New session" opens Claude** (Phase 21 C2, `web/src/lib/chat-prefs.ts`): `GET/PUT /api/me/agent-view {view}` with `view` `ask` (default: the three pictures Chat / Terminal / Chat + Terminal every time) · `chat` · `terminal` · `both`. A browser session or a token with `write`; audited as `settings.agent_view`. Settings → Terminal and "Make this my default" set it.
- **Tool calls in chat** (Phase 21 C5, `web/src/lib/chat-prefs.ts`, design `docs/design/chat-tools.md` §11): `GET/PUT /api/me/chat-tools {detail}` with `detail` `summary` (default: one line per tool call, opened by a click) · `expanded` (edits and command output arrive open, 12 lines each; reads, searches, fetches and helpers stay lines). A browser session or a token with `write` (`read` to read); anything else is a 400 `invalid_chat_tool_detail`; audited as `settings.chat_tools`. A client reads an unknown value as `summary`. Settings → Terminal sets it; "Expand all" in a chat does not.
- **Favourite layouts** (Phase 20, `web/src/server/layouts.ts`, design `docs/design/pane-layouts.md` §7): `GET /api/me/layouts` → `{favourites: [{id, name, shape, slots, updatedAt}], cap}`, oldest first (a browser session, or a token with `read`). `shape` is a split tree whose leaves are `{slot: n}` (`{dir: "h"|"v", ratio, a, b}` otherwise, ratios as in a layout tree); `slots[n]` is `{holds: "any"|"shell"|"agent", agent?, label?, cwd?: {base: "workspace"|"absolute", path}}`. At most 12 slots and 12 levels; an unknown key anywhere is refused, so a favourite never holds a command, an environment or a secret, and a client must read the list with the same strict reader (`parseFavourite` in `@tmuxedo/layouts`) and show names as text. `POST /api/me/layouts {name, shape, slots}` saves (201), or replaces your favourite of that name (200, `replaced: true`); `PATCH /api/me/layouts/{id} {name}` renames; `DELETE /api/me/layouts/{id}`. Changes need a browser session and are audited as `layout_favourite.create|replace|rename|delete` (counts only). Errors: 422 `invalid_favourite` (`details.issues [{path, code}]`), 409 `favourite_limit` (24 per account) or `name_taken`, 413 over 32 KiB. Restoring is the client's job: match open panes to slots (`planRestore`), open what is missing (`pane.split`, then `agent.start` for an agent slot, after the person confirms), then one `layout.arrange` with the explicit `root` and `extras: "new_tab"` when panes are left over. Nothing is closed.

- **Semantics:** press `prefix`, then one action key. The prefix pressed twice sends the prefix itself to the pane. Any other key cancels the prefix and is swallowed (`escape` included).
- **Actions:** `split_right` puts the new pane to the right (`pane.split {direction: "right"}`, layout `dir: "h"`). `split_down` puts it below. `detach` leaves the machine view: the web goes back to the machine list, the TUI exits its attach. `toggle_full_window` gives the mux the whole window (web: no page chrome, no page scroll; a TUI may map it to its own full-screen view or ignore it).
- **Key notation:**
  - A printable character is itself, case-sensitive (`%`, `"`, `C`), with no `shift+`.
  - Named keys are `left right up down enter tab space escape backspace home end pageup pagedown delete insert f1`…`f12`.
  - Modifiers come first, in the order `ctrl+`, `alt+`, `shift+` (`shift` only with a named key). Letters after ctrl or alt are lowercase: `ctrl+a`, `alt+x`, `ctrl+shift+left`.
- **Rules (enforced on `PUT`, 422 `invalid_keybindings` with `error.details.issues: [{path, code, message}]`):**
  - The prefix needs `ctrl` or `alt`.
  - Every action has exactly one key, and no two actions share one.
  - No action may use the prefix or `escape`.
- **Versioning:** a client must ignore unknown actions (new ones may appear while `version` stays 1). It must fall back to its built-in defaults if `version` is newer than it knows.
- **Later actions** (`rename_tab`, `rename_workspace`, `rename_pane`, queue 3c, tmux's `,` and `$`; `arrange` and `even_out`, Phase 20, web only so far: a client skips an action it does not know): when a stored document does not set one and its default key is already the prefix or another action's key, it takes the first free key of `f2`…`f9` instead of the document failing validation. Web (`effectiveKeybindings`, so `GET` already carries the moved key) and the TUI (for documents from servers that predate these actions) apply the same rule.
- `GET /api/me/appearance` → `{theme: "system"|"light"|"dark", terminalTheme: "auto"|"tmuxedo-dark"|"tmuxedo-light"|"solarized-dark"|"solarized-light"|"gruvbox-dark"}` (colours in `web/src/lib/appearance.ts`); a TUI may use `terminalTheme` for its chrome.

## Claude auth health

The runner watches whether the machine's `claude` is logged in and never reads or stores the credentials themselves. Signals:

- **Credentials** (Linux): `~/.claude/.credentials.json` exists → `ok`, absent → `missing`; checked every 5 s. A change of the file after a failure (a fresh `/login`) → `ok`. On macOS the login is in the Keychain, so the baseline is `ok` (reason `keychain (not inspected)`); `ANTHROPIC_API_KEY` in the runner's environment also counts as `ok`.
- **Screen**: a pane newly shows Claude's `Login expired` or `Please run /login` (case-insensitive) → `expired`.
- **Hooks**: `StopFailure` with `error_type` `authentication_failed`, `oauth_org_not_allowed` or `account_on_hold` → `failed` (reason = the error type); a completed turn (`Stop`) → `ok`.

**Organization turned the agent off.** A screen rule may say which state its text means (`auth.screen[].state`, default `expired`): Claude's "Your organization has disabled Claude subscription access for Claude Code" is `failed` with reason **`org_disabled`** (matched on a pane, wrapped lines joined, and on a chat's failed turn, `source: chat`). It is not a sign-in problem with the same account; clients show what fixes it (ask the organization's owner, sign in with another account, or an API key as a secret). It clears like any failure (a completed turn, or the credentials file changing).

The access token's short `expiresAt` is **not** a signal (decision 0001: Claude refreshes it itself). Each transition is a durable `event` with `type` `auth_ok` | `auth_missing` | `auth_expired` | `auth_failed`, `data` {`state`, `reason`, `source`: `credentials`|`screen`|`hook`}, and `paneId` of the pane that showed it (`""` for the credentials check). Web keeps the latest state per machine (`Machine.claudeAuthState`) for the badge, and Phase 6 push notifies on `auth_expired` / `auth_failed`.

## Plugins (Phase 12)

The plugin host (docs/design/plugins.md) on the wire. Everything is **additive** to v1; parsers drop junk instead of refusing a frame. Code and fixtures: `packages/protocol/src/plugins.ts` / `when.ts`, `runner/crates/protocol/src/plugins.rs` / `when.rs`, `packages/protocol/fixtures/plugins.json` and `when.json` (both sides check both files).

- **`hello.runner.plugins[]`** `{id, version, contract, state: enabled|disabled|incompatible|failed, agent?, agentVersion?, auth?: {state: ok|expiring|missing|expired|failed|unknown, reason, since, expiresAt?}, reason?}`, sent **beside** the legacy `claudeVersion` / `claudeAuth`, which stay byte-for-byte as before (dual-write). Readers take `plugins[]` first and fall back to the legacy fields for plugin `claude` (`helloAgentAuth`). A runner that sends `plugins` also answers `plugin.*` over the gateway, handles `plugin_action` and sends `ui`.
- **`welcome.features: ["ui"]`**: the gateway says it relays `ui` frames, and says so only to a runner whose hello has `plugins` (an older runner gets the old welcome). A runner sends no `ui` without it.
- **`config.plugins[]`** `{id, settings?: {launch?: {args}, skill?: {enabled}}}`, sent beside `config.claude.args`. A newer runner reads `plugins[claude].settings.launch.args` first; an older one reads `claude.args`. `skill.enabled` turns the generated tmuxedo skill on or off per plugin (absent = on; a config with no `plugins` at all, from an older web, leaves it off). `usage.enabled` turns AI usage on or off per plugin (absent = off; see "AI usage").
- **`ui`** (runner → gateway → clients) `{pluginId, scope: machine|pane, paneId?, items}` replaces the set for that (plugin, scope, pane); an empty `items` clears it. Items: `{key, kind: banner|toast|badge|quick_action|status_line, when?, target?: machine|agent, tone?: info|success|warning|danger, text|label, actions?|action?, dismissible?}` with the §9.3 limits (≤ 20 items, text ≤ 280, labels ≤ 40, ≤ 3 banner actions; the gateway drops more than 10 `ui` frames a second per machine). Items are declared in the plugin's manifest (`[[ui]]`); each **client** evaluates `when` with its own context (`auth.*`, `agent.status`, `agent.kind`, `machine.online`, `machine.kind`, `machine.name`, `settings.*`, `plugin.id`) and fills `{path}` text the same way; `target: agent` items apply to each pane whose agent is the plugin's. The gateway keeps the last set per slot for late joiners and posts it to web (`machine-state.ui`), so the machine page and an offline machine draw it too.
- **`plugin_action`** (client → gateway) `{id, pluginId, actionId, paneId?}`: control scope of the machine's owner; becomes `req plugin.action.invoke {plugin_id, action_id, context: {focused_pane_id?}}`; answered with `res {id}`; audited (`terminal.plugin_action`). The runner looks the action up in the manifest it holds, so a client names an action and never argv. Older runners: `not_supported` without a call. REST twin: `POST /api/machines/{id}/plugins/{plugin}/actions/{action} {paneId?}` (step-up or a `terminal` token; `machine.plugin_action` audited before it is sent). Client-side actions: `dismiss`, `settings.open`.
- **`plugin.*` methods** answer over the gateway too (web's method table: `plugin.list`, `plugin.action.list`, `plugin.log.list` read; `plugin.enable`, `plugin.disable`, `plugin.action.invoke` terminal).
- **Logins per plugin**: `auth_*` events may carry `data.plugin`; absent means `claude` (every event today). Web projects `AgentAuth(machineId, plugin)` from hellos and events beside `Machine.claudeAuth*` (still written for one release); `GET /api/machines/{id}` carries `agentAuth` and `surfaces` (each item evaluated on the projection, for the app), the sidebar `pluginBadges`.


## Plugin settings (Phase 17)

What an agent CLI is given besides its own configuration (design: docs/design/plugin-settings-ux.md). Additive to v1. Code and fixtures: `packages/protocol/src/plugin-set.ts`, `runner/crates/protocol/src/plugin_set.rs`, `packages/protocol/fixtures/plugin-set.json` (both sides), `runner/crates/schema` with `packages/protocol/fixtures/settings-schema.json` (native and WebAssembly).

- **Items.** `{id, kind: mcp|skill|subagent|command|instructions, name, version, definition}`. `name` is `[a-z0-9][a-z0-9-]{0,39}`. Definitions per kind (unknown keys are refused; `validateItemDefinition`):
  - `mcp`: `{transport: "stdio", command: [argv…], env?: {NAME: text | {"$secret": NAME}}, tools?: [...]}` or `{transport: "http", url, headers?: {Name: text | {"$secret": NAME}}, bearer?: {"$secret": NAME}, tools?}`. `command` is an argv list (1 to 64 arguments, each one line ≤ 4 KiB), never a shell string. `url` is http(s) without credentials in it.
  - `skill`: `{description, body}`; `subagent`: `{description, instructions, model?, tools?}`; `command`: `{description?, body}`; `instructions`: `{body}`. Bodies are Markdown ≤ 64 KiB; one definition ≤ 80 KiB serialised.
  - **Secrets by name only:** `{"$secret": "GH_TOKEN"}`. No frame, row or audit entry of this feature carries a secret value; the runner asks for the value when it launches the agent (Secrets, above).
- **Scopes and precedence** (`effectivePlugin`; user → machine → project, narrowest last):
  1. An item is visible where it is defined: everywhere (user), on one machine, or in one project.
  2. Of two visible items of the same kind and name, the narrower definition is the item there; the other is shadowed, even when the narrower one is off.
  3. An item is off until it is enabled for a plugin. Of the enable / disable rows that apply, the narrowest decides; a row broader than the item's own scope is ignored.
  4. A plugin's settings merge per key, one level into sections (objects); lists and plain values replace.
- **`config.pluginSet`** (web → runner; the effective set for this machine):

  ```json
  { "v": 1,
    "items":    [ { "id": "…", "kind": "mcp", "name": "github", "version": 3, "definition": { … } } ],
    "machine":  { "claude": { "items": ["…"], "values": { "launch": { "model": "opus" } } }, "codex": { "items": [] } },
    "projects": { "github.com/acme/api": { "claude": { "items": ["…"] }, "codex": { "items": [] } } } }
  ```

  `items` holds every item some plugin gets somewhere on this machine, once. `machine` is what each plugin gets in a folder that is in no listed project; `projects[key]` (keys as in `config.projects`) is the same for a project, and is listed only when it differs from `machine`. A project that is not listed gets `machine`; a plugin that is not in a scope gets nothing there. `values` are the plugin's settings (absent = none). ≤ 200 items and ≤ 200 projects.
  - Absent or `null` = unchanged. Not an object, or a `v` the runner does not know = refused, the previous set stays. Junk rows are dropped, never the whole set; an entry never names an item that is not in `items`.
  - **Compatibility:** an older runner ignores the field; a runner that knows it works unchanged with a gateway or web that never sends it (`fixtures/plugin-set.json` `absent`). The name is `pluginSet`, not `plugins`: `config.plugins[]` is Phase 12's per-plugin launch settings and stays as it is.
  - The runner today parses the set, drops what is over its limits (a definition over 96 KiB, settings values over the dialect's value limits) and keeps it in its state directory (`plugin-set.json`, mode 0600). The builders (below) run on each `config`.
  - **Who builds it:** web, from the user's items, on / off rows and settings (`web/src/server/plugins/`; REST in docs/api.md "Plugin settings"), for every `config` it sends: at admission and after every write (`machine.config`). It is always present, so a person who deletes everything sends an empty set that clears what the runner kept. Web keeps a set under 880 KiB and the gateway takes a `config` of at most 960 KiB (it was 64 KiB before Phase 17), under the runner's 1 MiB frame. Against a gateway that still refuses a large `config`, web sends the same config again without `pluginSet`, so launch and shell settings still land; the runner then gets the set at its next admission.
  - **Built-in settings schemas:** `runner/crates/runner/plugins/<id>/settings.schema.json` for `claude`, `codex`, `cursor` (`args` as `x-argv`, `model`, one `x-dangerous` switch each, `extras`, and for Cursor `keystroke`). Web imports them at build time and checks every settings write against them with the validator; `x-scope` is enforced by web on write. The runner compiles the same files in (beside the embedded packages, whose digests they do not change) and checks `values` against them again before use.
  - **Builders** (`runner/crates/runner/src/plugin_gen.rs`). The user's own CLI configuration is never written.
    - **Where:** `<state>/plugins/<id>/gen/<hash>/` (0700, files 0600) per scope: the machine's and each listed project's. `<hash>` covers the items, the values, the runner binary's path and the builder's version, so a changed set is a new directory and a running agent keeps the one it started with. A directory no scope names is removed after 7 days. `gen/index.json` maps a project key to its directory.
    - **At launch:** while the set has something for a plugin, its PATH shim runs `tmuxedo-runner cli plugin-gen <gen root>`, which resolves the folder's project (git remote, else a path project) and prints that scope's `argv.sh`; the shim sources it (`set -- <arguments> "$@"`). The generated arguments come after the manifest's own (`launch.prepend`, which holds the Phase 12 `config.plugins[]` launch args) and before what the user typed. So `values.args` is appended after the Phase 12 args, and where both name a flag the CLI's own rule decides (usually the later one). A shim without a set is byte for byte what it was.
    - **Claude Code:** `--mcp-config <dir>/mcp.json`, `--plugin-dir <dir>/plugin` (`skills/<name>/SKILL.md`, `agents/<name>.md`, `commands/<name>.md`), `--append-system-prompt-file <dir>/instructions.md` (blocks in the set's order, 16 KiB together).
    - **Codex:** `-c mcp_servers.<name>.command|args|env|url|http_headers|enabled_tools=…`, and one `-c developer_instructions=…` holding the generated tmuxedo skill (when on), the instruction blocks and one line per skill that names its `SKILL.md` in the directory (32 KiB together). Sub-agents and commands are not built for Codex yet.
    - **Cursor:** launch values only; every item is reported as left out.
    - **Values:** `model` → `--model`, the dangerous switch → `--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` / `--force`, then `args` (under the rule of `claude.args`: a value follows a flag). Values that do not fit the schema are not used at all. `extras` and `keystroke` are still read from `config.plugins[]`.
    - **Secrets:** a stdio server whose `env` names a secret is started as `tmuxedo-runner cli secret exec --mcp <plugin>/<server> NAME… -- <command>`: the value goes from the runner into that process only (the secret needs an `mcp` binding for the plugin and server). When the variable is not the secret's name, a fixed `/bin/sh -c 'VAR=$NAME; export VAR; unset NAME; exec "$@"'` sits in between; only checked names are in it. A remote (`http`) server that needs a secret is left out: neither CLI takes one without the value in a file or in the agent's own environment.
    - **`report.json`** in each directory: `{v, plugin, items: [{kind, name, status: "built" | "skipped", reason?}]}`, for the effective view.
- **The settings-schema dialect** (a plugin's `settings.schema.json`; one validator, `runner/crates/schema`, linked by the runner and loaded by web as WebAssembly through three exports `tsx_alloc`, `tsx_validate`, `tsx_free`). JSON Schema 2020-12 restricted to an allow-list:

  | Type | Keywords |
  |---|---|
  | every node | `type` (one string), `title`, `description`, `default`, `x-scope`, `x-dangerous`, `x-requires-plugin-version`, `x-requires-agent-version` |
  | `string` | `enum`, `pattern`, `maxLength`, `format: uri\|multiline\|markdown`, `x-secret-ref` |
  | `boolean` | none |
  | `integer`, `number` | `minimum`, `maximum` |
  | `array` | `items` (required; `{type: "string"}` with `enum`, `pattern`, `maxLength` only), `x-argv` |
  | `object` | `properties` (required), `required`, `additionalProperties: false`; the root and one level below it only |
  | the root | `$schema` (2020-12 only) |

  Everything else is refused with `keyword_not_allowed` (`$ref`, `$defs`, `oneOf`, `anyOf`, `allOf`, `not`, `if`, `patternProperties`, `const`, `minLength`, `maxItems`, … the fixture lists each). Unknown settings are always refused, `null` is not a value, a plain string is one line, `format: uri` means http(s), and an `x-secret-ref` string is a secret's name. Patterns use the linear-time engine (no look-around, no back-references) and search unanchored.
  - **Hard limits:** schema ≤ 64 KiB and ≤ 8 nested containers; ≤ 64 properties per object, ≤ 256 nodes, ≤ 32 patterns of ≤ 256 bytes each compiling to ≤ 1 MiB; `enum` ≤ 64 choices of ≤ 256 characters; `maxLength` ≤ 65536 (default 4096 for a line, 2048 for a uri, 65536 for multi-line text); lists ≤ 256 items; a value ≤ 256 KiB and ≤ 3 nested containers.
  - **Answer:** `{"schema": [{path, code, message}], "value": [...]}` with JSON Pointer paths; the value is checked only against a clean schema.

### Pinned packages

A local MCP server's `command` may fetch a package at launch. Web and the runner read it the same way (`mcpPackage` in `packages/protocol/src/mcp-pin.ts`, `mcp_pin::package_pin` in the runner, on `packages/protocol/fixtures/mcp-pin.json`): the launcher (`npx`, `bunx`, `pnpm dlx`, `yarn dlx`, `npm exec`, `uvx`, `uv tool run`, `pipx run`, `docker`/`podman run`), its options skipped by a table of the ones that take a value, then the package from `-p`/`--package`, `--from`, `--spec` or the first plain argument. Pinned means one exact version: npm `pkg@1.2.3` (semantic version, no `v`), PyPI `pkg==1.2.3` or `pkg@1.2.3` (PEP 440, no wildcard), an image `name[:tag]@sha256:<64 hex>`. A local path is not a package launch.

The runner passes the argv through element by element (no shell), so what runs is what was pinned. It builds an unpinned launch too (web refuses new ones; older ones keep working) and its `report.json` row says what it found: `"package": {registry, name, version, pinned}` and, when not pinned, `"warning"`. The builder's version (part of every generated directory's hash) is 2.

## Needs you (Phase 13)

"Needs you" through the plugin contract (docs/design/multi-agent-workflows.md §2). Everything is **additive** to v1. Code and fixtures: `packages/protocol/src/needs.ts`, `runner/crates/protocol/src/needs.rs`, `packages/protocol/fixtures/needs.json` (both sides check it); the runner's side is `runner/crates/runner/src/needs.rs`.

- **Features.** A runner that sends `needs` and answers `needs.respond` lists `"needs"` in `hello.runner.features`; a gateway that relays `needs` lists `"needs"` in `welcome.features`. A runner sends no `needs` frame without it.
- **`NeedsYou`** `{id, paneId ("" = machine), chatId?, plugin, kind: permission|question|plan|login|error|limit|review|file, title (≤ 120, masked), detail? {tool?, input?, suggestions?, text?, message?, questions?, markdown?, upload? {project, requestId} (file)}, context?[], risk: normal|high, riskClass?: git_push|install|admin|delete_outside|edit_outside|network|credentials, why?, responses[], answerVia: hook|keystroke|action|session|none|upload (a file item, and only it), fingerprint? (sha-256 hex, keystroke only), source: hook|screen|report|core|chat, createdAt, expiresAt?}`. A response is `{id, kind: approve|deny|reply|choose|answers|approve_plan|action|open|dismiss|upload (file items only), label (≤ 40), requires: none|control|stepup, options?[{id, label}] (choose), actionId? (action)}`. An item from `pane.report_agent` (`source: report`) never offers anything but `open`/`dismiss`.
- **`needs`** (runner → gateway → clients) `{paneId, items}` replaces the set for that pane (≤ 20 items; an empty list clears it). The gateway checks each item (`parseNeedsFrame`), keeps the last set per pane for late joiners, and posts it to web (`machine-state.needs`), which projects it (`pane.needs`, `machine.needs`).
- **Events.** `needs_opened {item}` and `needs_resolved {id, kind, plugin, by: remote|terminal|expired|agent|screen_changed, responseId?}` are durable (stored, pushed: `needs_opened` drives the notification). The older `blocked {kind, promptId}` is still sent; web does not push it for a machine whose runner lists `needs`.
- **`needs.respond`** (a runner API method; mutating, so control scope; refused on the runner's local socket, which anything in a pane can reach) `{itemId, paneId?, chatId?, responseId, text? (≤ 2000, no control characters), optionId?, answers?, by?, fresh_stepup?}` → `{type: "needs_respond", outcome: accepted|stale|unknown}`. Routing by `answerVia`:

  | `answerVia` | What the runner does | Staleness |
  |---|---|---|
  | `hook` | releases the held hook with the plugin's reply template (`approve` → allow, `deny` → deny with `text` as the message) | the item id is the held prompt's id: gone or answered → `stale` |
  | `keystroke` | re-reads the screen's prompt region (the last `[keystroke] rows` lines) and compares its fingerprint; then types only keys on the plugin's allowlist (`choose` → the option's key, `deny`/Cancel → Escape, `reply` → one line of text and Enter where the plugin allows it). The region must change within 2 s. | fingerprint differs → `stale`, nothing typed, the item closes `by: screen_changed`; no change in 2 s → `unknown` |
  | `action` | runs the manifest action (`plugin.action.invoke`), e.g. a login | the item must be open |
  | `session` | a chat's item: `chat.respond` with the same words | exact, by id |
  | `none` | refused (`answer_in_terminal`): opened from the client, answered at the terminal | — |

  Approving a `risk: high` item needs `fresh_stepup: true`, which **only web** sets (a step-up in the last 5 minutes; the gateway drops a client's own `fresh_stepup`). Keystroke answers are per plugin: the manifest's `[keystroke] {kinds, keys, reply, default_on, rows, asks}` says which kinds, which hook signals mean a question is being asked (keys answer only those: never a wait read from the screen alone, nor a permission whose hold expired), which keys (single digits/letters and Escape; a permission only `y`/`n`/Escape, always opt-in), and `config.plugins[].settings.keystroke.enabled` is the person's switch (absent = the manifest's `default_on`).
- **Gateway RPC `machine.respond`** `{machineId, ...needs.respond params}` → `{outcome}`. For a runner without the `needs` feature, a `hook` permission's `approve`/`deny` falls back to the `decision` frame (`machine.decision` semantics); anything else is `not_supported`.
- **Web** `POST /api/needs/respond {machineId, itemId, responseId, paneId?, chatId?, text?, optionId?, answers?}`: ownership, the `terminal` scope for tokens, the response's `requires` (step-up within 15 minutes; a high-risk approve within 5), audited before it is sent (`needs.respond`, never the text: its length), then `machine.respond`. 200 `accepted`, 202 `unknown`, 409 `stale` (`needs_stale`), 409 `machine_offline`, 403 `step_up_required` / `fresh_step_up_required`. `POST /api/agents/decision` stays as an alias (`behavior` allow|deny → `approve`|`deny`, `promptId` → `itemId`). `GET /api/needs` lists every open item across the user's machines and chats (pane items, a legacy held prompt as a `hook` permission item, chat items as `session` items).
- **Push categories** (fixed; iOS registers them at build time): `NEEDS_APPROVE_DENY` (Approve / Deny), `NEEDS_REPLY` (a text reply), `NEEDS_OPEN` (opens the app). An item gets the richest category its responses fit (`needsCategory`); **high risk never gets one with Approve** (D-22: only from the full sheet). A notification action answers only what its category allows (`notificationResponse`), and the server checks it again: the app's device-key path refuses approving a high-risk item. **Coalescing:** one notification per pane (or chat) per minute; items that open within the minute are counted and the next one says "3 more waiting".
- **File requests (Phase 15).** `tmuxedo files request "what"` opens a `file` item in the asking pane's set (the machine's when not asked from a pane): `answerVia: upload`, `source: core`, `plugin` = the pane's agent plugin (else `tmuxedo`), title `<Agent> wants a file: <what>`, `detail {text, upload {project, requestId}}`, responses `upload` ("Upload a file") and `dismiss` ("Not now"), both `requires: none`, `expiresAt` 24 h. **Answered by an upload**, not by `needs.respond`: an upload whose `files.upload_init` names `request_id` and lands in the same project closes it (`needs_resolved by: remote, responseId: upload`). `needs.respond` with `dismiss` closes it (`responseId: dismiss`); with `upload` it is refused (`invalid_params`). After 24 h it closes `by: expired`; when its pane goes, `by: agent`; the same words again in a pane replace it (`by: agent`); at most 5 per pane (the oldest `expired`). Kept in `<state>/file-requests.json` across runner restarts. A `file` item is `NEEDS_OPEN` (no notification action answers it); its push opens `/machines/{id}?sheet=uploads&pane=…&request=<requestId>`. Web: `POST /api/needs/respond` sends a file item's `dismiss` to the machine without a step-up (it changes nothing there) and refuses `upload`; the Files sheet lists the folder's open requests from `GET /api/needs` and its next upload answers `?request=`, else the newest. Old clients drop a `file` item (an unknown kind) and still get its push.
- **Hook callers (not-yet-thought-of.md #1).** The runner reads the caller's pid from the kernel on its local socket (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS) and counts a `hook.event` only when the caller descends from the pane's process and sits at most 3 parents below the pane's topmost agent process, which must be one of the plugin's agent labels (a chat's hooks: at most 3 below the chat's agent child). Anything else is logged and answered `{}`: no status, no prompt, no item.
- **Links.** A plugin's `[links] {open_url, login}` lists hosts (`claude.ai`, `*.anthropic.com`). An `open_url` event carries `verified: true` (from the pane's agent, to a listed host) or `verified: false` with `reason: not_from_agent|host_not_allowed`; only a verified link to a `login` host gets the login callback relay. Web, the app and the TUI show an unverified link as "Unverified link to <host> from a process in <pane>", with the full host and no auto-open. An event without `verified` (an older runner) is shown as before.

## Helpers (Phase 18)

A **helper** is a sub-agent that a pane's agent runs inside its own process (Claude Code's sub-agents, Codex's spawned threads). The plugin's manifest maps the CLI's hooks to the normalised events `subagent.started` and `subagent.completed` (design/plugins.md §5.3); the runner keeps the list per agent. Types, limits and the lenient reader: `packages/protocol/src/agents.ts` and `runner/crates/protocol/src/subagents.rs`; fixtures `packages/protocol/fixtures/subagents.json`, checked from both sides.

- **`Agent.children`** (optional, in `status` frames and in the topology's `panes[].agent`): `[{id, name, status: working|done, since, startedAt?, parentId?, model?, roleId?, needs?}]`, oldest first. Absent when there are none and from older runners, so every reader treats a missing field as no helpers. `name` is what the CLI calls the helper (its type or role), `since` is when the status began, `parentId` is set for a helper started by another helper when the CLI says so, `roleId` is set when the helper's name is one a builder gave a role effective in the pane's folder (below; a Claude helper may also arrive as `tmuxedo-items:<name>`), `needs` is reserved (not sent yet). The pane's own `status` stays the root agent's: a helper never moves it and never releases a held prompt.
- **Bounds.** At most 16 children per agent (the oldest finished one makes room, then the oldest working one). A finished helper stays on the list for 10 minutes. A helper whose end is never reported is dropped after 6 hours. The list ends with the agent: session end, the agent's process leaving the pane, or the pane exiting. Ids are cut to 128 characters, names and models to 80.
- **Ids.** When the CLI's hook names no id, the runner numbers the helper (`helper-1`, …) and pairs a stop with the newest working helper of that name, else the newest working one.
- **Events.** `subagent_started` and `subagent_completed` are durable `event`s with `data` `{kind, childId, name, parentId?, model?, roleId?}`. They are stored like any event and raise no push. A helper that is dropped (stale) or ends with its agent raises no event.
- **Restart.** The runner saves the list with the pane; after a runner restart the helpers of a process that is still running are back on the agent. A new process (holder restart, resume) starts with none.
- **Gateway.** Forwards the frames as sent and keeps the cached topology's `panes[].agent` as the last `status` frame left it, so a client that joins later sees the same agent and helpers.
- **Clients.** Web shows the number at work ("2 helpers") on the session card and in the pane header, only when there is one. The dashboard's first paint comes from the stored projection, which has no helpers; the count appears with the next live `status` frame.

## Launch and shell configuration (web side)

The `config` frame's content is owned by web: **Settings → Terminal** holds the user's defaults (`GET/PUT /api/me/terminal-config`, `{claude: {args}, shell: {aliases, env, rc}, workDir}`), and a machine's **Terminal settings** page holds an optional override (`GET/PUT /api/machines/{id}/terminal-config`, `{override: {claude?, shell?, workDir?} | null}`). An override **replaces whole sections**: a machine that sets `claude` uses its own args, one that sets `shell` its own aliases, env and rc, one that sets `workDir` (even `""`, the default) its own folder; anything it leaves out comes from the user's defaults. Writes need a signed-in session stepped up in the last 15 minutes (these run in every new pane), are validated with the rules in the `config` row above (web refuses with 422 `invalid_terminal_config` and `details.issues`), audited by name and size only (env values and rc never enter the audit log), and pushed through `machine.config` to every affected machine. Code: `web/src/lib/terminal-config.ts`, `web/src/server/terminal-config.ts`.

## Roles (Phase 18)

A **role** is a reusable agent definition (design/agent-team-builder.md §2.1). Web, the app and the runner read a role document the same way and take the same hash:

- `packages/protocol/src/roles.ts` (`parseRole`, `canonicalRole`) and `runner/crates/protocol/src/roles.rs` (`parse_role`, `Role::canonical`); fixtures `packages/protocol/fixtures/roles.json` (`npm run fixtures` regenerates them), checked from both sides.
- **Strict.** Unknown fields, wrong types and broken limits are problems, each reported with the dotted path of its field (`tools.allow.1`); both sides report the same paths in the same order. Nothing is dropped or repaired.
- **Fields.** `schema` (1), `id` (`[a-z0-9-]{2,32}`), `name` (one line, ≤ 60), `avatar {glyph, color}` (colour from a fixed palette of eight), `purpose` (one line, ≤ 200), `instructions` (≤ 16 KiB of text; no control characters but newline and tab), `cli` (`any` or a plugin id; default `any`), `model` (per plugin id), `access` (`read-only` | `edit` | `edit+shell` | `full`; default `read-only`), `tools {allow, deny}`, `mcp`, `skills`, `secrets` (names only), `delegates` (other role ids), `limits {max_subagents ≤ 16, max_depth ≤ 4}`.
- **Canonical text.** JSON with keys sorted at every level and no spaces; `schema`, `cli` and `access` always written; empty optional parts left out. A role's **hash** is the SHA-256 (hex) of that text, so the order fields arrive in and defaults written out by hand never change it.
- A plugin's manifest says what its CLI can do with a role as a helper: `[multi_agent]` (design/plugins.md §5.3).

**`config.roles`** (gateway → runner, in the `config` frame beside `pluginSet`; absent = unchanged, an older runner ignores it): `{v: 1, roles: [{ref, version, hash, builtin?, role}], machine: [ref…], projects: {<project key>: [ref…]}}`. Each role is sent once, as its canonical text parsed; `ref` is the library's id (`builtin:<id>` for a starter role). `machine` lists the refs effective in a folder of no listed project, each listed project its own whole list: web has already applied the nearest-scope rule (project, machine, account, built-in), and only projects with a role of their own are listed. Limits: 200 roles, 200 projects, 512 KiB of JSON (web drops the last projects, then the machine's last roles, and logs it; the runner refuses a larger set whole and keeps the previous one). The runner re-parses every role strictly and drops junk rows (a role that does not parse, a repeated ref, a ref to nothing, a second ref for the same role id in one list). Code: `web/src/lib/roles.ts` `roleSetFor`, `web/src/server/roles-config.ts`; `runner/crates/protocol/src/role_set.rs`, `runner/crates/runner/src/role_set.rs` (kept as `roles.json`, 0600). A role change pushes the config to every live machine.

- **Builders.** For each scope, every role that is not a built-in is offered to each plugin whose `[multi_agent]` is `native` and hosts the role's `cli`. Claude Code: `agents/<id>.md` in the generated plugin directory (front matter `name`, `description` = purpose or name, `model` when it is one safe word, `tools` = `tools.allow`, else from `access`: read-only `Read, Grep, Glob`, edit adds `Edit, Write`, edit+shell adds `Bash`, full leaves it out; body = instructions). Codex: `-c agents.<id>.description=…` and, with a model, `-c agents.<id>.config_file=<dir>/agents/<id>.toml` holding `model`. Cursor: nothing. A role and a `subagent` item with the same name: the role is written, the item skipped. `report.json` has a row per role `{kind: "role", name, roleId, status, reason?, notApplied?}`; `notApplied` names the parts the CLI's helper does not carry (`instructions`, `model`, `tools`, `access`, `mcp`, `skills`, `tools.deny`, `secrets`), which stay the parent pane's.
- **`agent.start {role}`** (runner API): the role the id means in the pane's folder, checked to run on the agent (`cli` is `any` or the agent's id). Its arguments go after the shim's and before the caller's `args`: `--model <m>` (one safe word, never starting with `-`); Claude: `--append-system-prompt-file <state>/roles/<hash>.md` (0600); read-only access is Claude's `--permission-mode plan` and Codex's `--sandbox read-only`, Codex edit and edit+shell `--sandbox workspace-write`; nothing else, and never a plugin's dangerous switch. The reply adds `role {id, ref, version, applied, notApplied}`. Errors: `unknown_role` (no such role here, or no `config.roles` yet), `role_cli_mismatch`, `invalid_params` (not a string), `role_unavailable` (the instructions file could not be written).

## Glances (dashboard thumbnails, Phase 11)

A glance is a pane's **visible screen** as styled text runs, for the dashboard's live thumbnails (design: [dashboard-and-terminal-page.md §2.2](design/dashboard-and-terminal-page.md), decision D-15). Code: `packages/protocol/src/glance.ts` (decode, widths), `runner/crates/runner/src/glance.rs` (encode), `gateway/src/hub.ts` (refcounts).

| Direction | Frame | Meaning |
|---|---|---|
| client → gateway | `{t:"glance", panes:[{machineId?, paneId}], hz?, rows?, cols?}` | **Replaces** this socket's glance set (empty = none). At most 64 panes; `hz` is clamped to 0.1..1 (default 0.5) and is also the most often this socket is sent one pane's glance. `rows` and `cols` (both or neither) ask for a crop: see "Crop and rate per socket". `observe` scope is enough (a glance shows what observing shows). On a socket bound to one machine `machineId` may be left out and any other machine is ignored; on a machine-list socket (ticket without `mid`) it names one of the user's machines. |
| gateway → runner | `{t:"glance", paneIds:[…], hz}` | The union of every client's set for this runner, and the highest `hz` asked. Sent when either changes and after `welcome`; `paneIds: []` stops all glance work. Sent **only** to runners whose `hello.runner.features` contains `"glance"`. |
| runner → gateway | `{t:"glance", paneId, rev, cols, rows, cursor?:{x,y}, lines}` | The pane's visible screen, sent when it **changed** since the last glance for that pane (a hash of the encoded screen, cursor and size), checked at most `hz` times a second, never in the middle of a synchronized-output frame. `rev` counts glances per pane. |
| gateway → client | the runner's frame plus `machineId`, cropped if the socket asked | Only to sockets whose set contains that pane, and only when the socket's user owns the machine. A client that adds a pane gets the gateway's last glance for it at once. |

**Encoding.** `lines` has one entry per row; a row is an array of runs `[text, fg?, bg?, attrs?]` with trailing default values left out. `fg`/`bg`: `null` = default colour, `0..255` = palette index, `"#rrggbb"` = true colour. `attrs`: bit 0 bold, 1 dim, 2 italic, 3 underline, 4 inverse. Trailing blank cells (a space with the default background and no underline/inverse) are dropped, so a blank row is `[]`. A wide character covers two columns. A pane larger than 200 × 60 is cropped to its **bottom** 60 rows and **left** 200 columns (`cols`/`rows` are the cropped size; `cursor` is left out when hidden or cropped away). Golden fixtures: `packages/protocol/fixtures/glance.json` (input bytes → frame), checked by the runner's encoder and the TypeScript decoder.

**What a glance never contains.** Scrollback (only the visible screen, at scrollback offset 0, main or alternate screen, whichever is shown) and anything not already on that screen. It is built from the runner's per-pane emulator, the same one `SNAPSHOT` and `pane.read` read, which is fed from the runner's single read of the holder: the output choke point where the secret masker sits ([secrets-and-env.md §5](design/secrets-and-env.md#5-output-side-redaction-the-single-choke-point)). A glance therefore can never show more than an observer's attach would.

**Crop and rate per socket** (the phone's home, [mobile-home.md §10](design/mobile-home.md)). Both are done by the gateway for each socket; the runner's side does not change, and it still sends each pane at the highest `hz` any viewer asked.

- **Rate.** A socket is sent one pane's glance at most once per `1/hz` seconds of its own `hz` (a frame is let through from 90 % of that interval, since the runner's timer is not exact). A glance that comes sooner is not queued: the gateway keeps one mark per socket and pane and, when the interval ends, sends whatever is the newest glance then. So a slower viewer misses the frames in between and never the last one. A socket with more than 256 KiB waiting in its send buffer is treated the same way and looked at again every second. Per runner the gateway holds one glance per pane and at most 8 cropped copies of it (one per size asked), all forgotten at refcount 0.
- **Crop.** With `rows` and `cols` (whole numbers of at least 1; values above 60 and 200 are clamped to those; one without the other, 0, a negative, a fraction or a string is `error bad_frame` and the set is left as it was) every glance for that socket is cut to the **bottom `rows` rows that have content** and the **left `cols` columns**. Trailing blank rows are dropped first; a row with a background colour or inverse counts as content and so does the cursor's row. A character that does not fit ends its row whole, so a wide character is never split and a combining mark stays with its character (at most 8 marks per character are kept). Control characters are left out. The result is an ordinary glance frame: `rows` is the number of rows kept (a blank screen is `rows: 1`, `lines: [[]]`), `cols` the smaller of the pane's and the crop's, colours and attributes as sent, `rev` and `paneId` unchanged. `cursor` is moved to its row in the crop and left out when that row is not in it; its `x` is the pane's own, so `x >= cols` means the cursor is right of the crop. A glance the gateway cannot decode is not sent to a socket that asked for a crop. Code: `cropGlance` in `packages/protocol/src/glance.ts`.
- A crop is a view of the same glance the same socket may already have whole: it passes the same owner check and adds no access. An older gateway ignores `rows` and `cols` and sends whole screens at the highest rate asked, so a client keeps its own crop and its own minimum interval. Metric: `tmuxedo_gateway_glance_held_total` (times a glance was held for a socket).

**Refcounts.** The gateway keeps, per runner, the set of client sockets glancing at each pane (in memory, like attaches). A socket that closes, or sends a set without a pane, drops its references; a pane whose count reaches 0 leaves the union and its cached glance is forgotten. Lost on gateway restart and rebuilt as clients reconnect and resend their sets; a runner that reconnects gets the union again after `welcome`. Metrics: `tmuxedo_gateway_glance_panes` (panes with a count above 0, all runners), `tmuxedo_gateway_glance_frames_total`, `tmuxedo_gateway_glance_bytes_total{direction}`.

**Old runners** list no `glance` feature: the gateway never sends them the frame, and clients show the pane's projected `title` (and agent status) instead of a thumbnail. Old clients never send `glance` and are unaffected; an old gateway answers a client's `glance` with `error bad_frame`, which the client treats as "no thumbnails".

**Tickets.** `POST /api/gateway/ticket {purpose: "glance"}` (no `machineId`) mints a one-minute `observe` ticket **without** `mid` for the dashboard's machine-list socket (audited `terminal.glance_ticket`). For an App Store review account it also carries `demo: [<its demo machine ids>]`, so the gateway plays the demo machine for that socket (see "Demo machines").

**Cost bound** (measured by the Phase 11 load test): see [dashboard-and-terminal-page.md §2.2](design/dashboard-and-terminal-page.md) "Measured".

## Login callback relay

A CLI's browser login in a pane listens on the machine's loopback for the OAuth redirect: Claude Code's `/login` (`http://localhost:<random port>/callback`; panes always have `BROWSER`, so Claude picks this flow) and `codex login` (`http://127.0.0.1:1455/auth/callback`, fixed port; decision 0003 §Login). The user's device cannot reach that address, so the browser ends on an error page. The runner, holder, panes and the CLI's server share one network namespace (the pod on hosted machines, the host on BYO), so the runner delivers that final address for the user. Code: `runner/crates/runner/src/login_relay.rs`, `packages/protocol/src/login-callback.ts`, `gateway/src/hub.ts` (`loginCallback`), `web/src/server/login-callback.ts`.

1. **Detect.** For every `open_url`, the runner reads the URL's own query: a `redirect_uri` of `http://localhost:<port>/…` or `http://127.0.0.1:<port>/…` (explicit port, no userinfo), a non-empty `state` (≤ 512) and `response_type` or `client_id`. Then `data.callback` = `{host, port, path, state}` (host lowercase, exactly as in `redirect_uri`; path without query) and the runner remembers it for that pane for **15 minutes** (at most 4 per pane; the same callback again refreshes it). The gateway passes a valid `callback` through and drops a malformed one.
2. **Paste.** The client (web toast, app sheet, `tmuxedo login-callback`) sends the address the browser ended on: client frame `login_callback {id, paneId, url}` (control socket), or web's `POST /api/machines/{id}/login-callback {paneId, url}` → RPC `machine.login_callback` → gateway frame `login_callback {id, paneId, url}`.
3. **Check (runner), in order:** at most **6 attempts per pane per minute** (`rate_limited`); an `http` URL on `localhost`/`127.0.0.1` (`invalid_url`); the pane has a remembered callback (`no_pending`); one with the same scheme, host, port and path (`mismatch`); and the same `state` (`state_mismatch`); not delivered before (`already_used`); younger than 15 minutes (`expired`); its port is listening here, read from `/proc/net/tcp{,6}` on Linux (127.0.0.1/0.0.0.0, else ::1/::), a connect probe elsewhere (`not_listening`).
4. **Deliver.** The callback is spent (single use, even if the CLI then says no), and the runner makes **one** `GET <path>?<query>` to the listening loopback address with `Host: <host>:<port>` (what the browser would have sent) and `Connection: close`, no redirects followed, **10 s** for everything. `res.result` = `{ok, status, excerpt?}`: `ok` for 2xx/3xx (both CLIs redirect to a success page); `excerpt` only for errors, at most 200 characters of the body with tags stripped and the request's own values and anything secret-shaped masked. A failed GET is `res.error` `timeout` or `unreachable`.

| `res.error.code` | Meaning |
|---|---|
| `invalid_url` | Not an `http://localhost:<port>/…` or `http://127.0.0.1:<port>/…` address. |
| `no_pending` | No login link with a localhost callback was opened in that pane in the last 15 minutes. |
| `mismatch` | Host, port, path or scheme differ from the pane's callbacks. |
| `state_mismatch` | Right place, other login attempt (wrong or missing `state`). |
| `expired` / `already_used` | Older than 15 minutes / delivered once already. |
| `not_listening` | The CLI stopped waiting (nothing listens on the port). |
| `rate_limited` | Six attempts this minute for this pane. |
| `timeout` / `unreachable` | The GET did not finish in 10 s / failed. |
| `not_supported` | (gateway) The runner's hello lacks the `login_callback` feature. |
| `machine_offline` | (gateway) No runner. |

**Who may.** The client frame needs `control` scope on a socket bound to the machine, from its owner; the gateway answers `forbidden_scope` otherwise and forwards only to a runner with the `login_callback` feature (`not_supported` otherwise, never a silent timeout; it waits 15 s). Web's route needs a session stepped up in the last 15 minutes or a token with the `terminal` scope, and ownership.

**The URL is a credential.** Its `code` (with the CLI's PKCE verifier, which never leaves the CLI) becomes a login, so it is never logged or stored anywhere: the runner logs the host and the refusal code or HTTP status, the gateway the host and outcome, and web audits **by host only** (`localhost` / `127.0.0.1`, never port, path or query): `terminal.login_callback` before the RPC and `terminal.login_callback_result` with the outcome after, for the web route; one `terminal.login_callback` row with the outcome, reported by the gateway through `/api/internal/terminal-audit`, for a client socket.

**Golden fixtures:** `packages/protocol/fixtures/login-callback.json` (detection, matching, the client and gateway frames, result shapes and refusal codes), checked by `packages/protocol/test/login-callback.test.ts` and by the runner (`login_relay.rs` `golden_fixtures`, `runner/crates/protocol/tests/fixtures.rs`).

**Old runners** list no `login_callback` feature and never mark `callback`; clients show no paste affordance for them. Old clients ignore `callback`.

## Names

Workspaces, tabs and panes have names people give them (queue 3c). Code: `packages/protocol/src/names.ts` and `runner/crates/protocol/src/names.rs` (the same rules, both checked against `packages/protocol/fixtures/names.json`), `runner/crates/runner/src/api.rs` (`*.rename`), `web/src/lib/names.ts`, `mobile/lib/names.ts` (a copy held to the same fixture).

- **Where.** `Workspace.name` and `Tab.name` (always set) and `Pane.label` (absent when the pane has no name; then clients show its `title`). Runners with the hello feature `names` send `label` and apply the rules; older runners never send `label`, and web cleans workspace and tab names it projects from them.
- **Rules.** Any whitespace becomes a space; C0/C1 controls, DEL, the bidi overrides, isolates and marks, zero-width space and BOM are removed (ZWJ and variation selectors stay); runs of spaces collapse; trimmed; at most **64 code points** (never splitting a surrogate pair), trimmed again.
- **Empty means the default.** A workspace `w<n>` goes back to `default` (n = 1) or `n`; a tab `w<n>:t<m>` to `m`; a pane loses its label. `workspace.rename` / `tab.rename` take `label: null` for that (the parameter must be present); `pane.rename` with no or an empty `label` clears it.
- **Paths.** Web, the dashboard and the app use `PATCH /api/v1/machines/{id}/{workspaces|tabs|panes}/{id}` `{label: string|null}` (session or `write` token; a session needs same-origin, no step-up: a name is not terminal input, so a watching window may rename). Web cleans it, fills the default for workspaces and tabs itself (runners from before 3c require a label there), audits `api.call` with `params.label` and `before` (the projected name), and forwards the method. The TUI sends `req workspace.rename|tab.rename|pane.rename` on its control socket (control needed, as for any mutating `req`); the gateway forwards it and reports `terminal.rename {via: "socket", method, targetId, before, name, outcome}` to web through `/api/internal/terminal-audit` once the runner answers (`outcome`: `renamed`, the runner's error code, `timeout` or `machine_offline`).
- **Concurrent renames.** The runner applies them in arrival order and every caller gets its `res`; the last one is what the next `topology` carries to every client. Clients keep a rename the user is typing when another client's rename arrives (the draft is not overwritten); saving it is simply the next rename.
- **Offline.** A rename needs the runner: web answers 409 `machine_offline`, and every client says so instead of opening its editor.
- **Where names show.** Terminal tabs, the pane header and the pane switcher; dashboard cards (text and accessible name); the sidebar's agent rows; "Needs you" items (`paneLabel`); push titles (`Claude needs you · <name> · <machine>`, masked like every push field); the TUI's tab bar, sidebar and status line; the app's machine, terminal, agents and machines screens.

## Agent chat

A **chat** drives an agent CLI in its structured mode (Claude Code: `claude -p --input-format stream-json --output-format stream-json`) instead of in a terminal; design: [design/agent-chat.md](design/agent-chat.md). Runner hello feature: **`chat`**. The runner owns the conversation: it translates the CLI's stream into the events below, masks every string, numbers each durable event and keeps it in the session's log on the machine for **30 days** (`<state>/chat/<session>/events.ndjson`). Web stores no chat content.

**The agent process.** The chat's CLI is a holder **pipe child** (holder op `spawn_pipe`: pipes instead of a PTY, the same ring and offsets as a pane), so a runner restart or self-update never kills a chat mid-turn: the new runner re-attaches from the last stdout offset it handled, and no event is lost or logged twice. An idle chat nobody watches **parks** after 30 minutes (the CLI exits; the next message resumes the same session id). A holder from before `spawn_pipe` never answers it: `chat.start` fails with `holder_outdated` (restart the service).

**CHAT frame** (binary kind **4**), the pane-frame header reused:

```
kind 4 · flags bit0 GAP (events before this one were pruned or lost), bit1 EPHEMERAL · req (0 = live, else the chat_attach it replays for)
id = chat session id ([A-Za-z0-9_-]{8,64}) · offset = the event's seq · payload = one event, UTF-8 JSON
```

Durable events have `seq` 1, 2, 3 … per session. A **token delta** (`message.delta`) is EPHEMERAL: never logged, `seq` 0 in its JSON, and the frame's offset is the last durable seq before it. The gateway reads only the header, as for pane bytes (design §3.4); end-to-end encryption can wrap the payload later with no gateway change.

**Watching a chat** (observe scope is enough):

| Frame | Direction | Meaning |
|---|---|---|
| `chat_attach {sessionId, from?}` | client → gateway | replay durable events from seq `from` (default 1), then live |
| `chat_attach {req, sessionId, from}` | gateway → runner | the same, with the gateway's `req`; only to runners with the `chat` feature |
| CHAT frames with `req` | runner → gateway → that client | the replay |
| `chat_attached {req, sessionId, ok, lastSeq, code?}` | runner → gateway | after the replay; `ok: false` with `code: chat_not_found` for a session the runner does not have |
| `chat_attached {sessionId, ok, lastSeq?, code?}` | gateway → client | the replay is complete (or refused: `chat_not_found`, `not_supported`, `machine_offline`; offline is kept and attached when the machine returns) |
| CHAT frames, `req` 0 | runner → gateway → watchers | live |
| `chat_detach {sessionId}` | client → gateway → runner | stop watching (the runner hears it when the last watcher leaves) |

The gateway keeps, per client per chat, `sent` = the last seq forwarded. A live durable frame at or below `sent` is a duplicate (dropped); one beyond `sent + 1` is a hole: never forwarded, the client is re-attached from `sent + 1`. An ephemeral frame goes only to a live watcher whose `sent` equals its offset. A client socket whose send buffer passes 4 MiB stops receiving chat frames and is re-attached from `sent + 1` when it drains. Every watcher is re-attached from its `sent + 1` after a runner reconnect. Clients never send CHAT frames (refused).

**Methods** (`req`, the runner API; reading needs observe, everything else control; web's REST twin: docs/api.md "Agent chat"): `chat.start {agent?, cwd?, mode?, text?, title?, client_msg_id?}`, `chat.list`, `chat.get {session_id}` (with the open needs-you items), `chat.read {session_id, from_seq?, limit?}`, `chat.send {session_id, text, client_msg_id?}` (≤ 64 KiB; a resent `client_msg_id` answers the first `seq` with `duplicate: true`; sent during a turn it is queued and shown at once with `queued: true`), `chat.interrupt`, `chat.mode {mode}` (`ask_first` · `plan_first` · `auto_edit` · `auto_approve`, mapped by the plugin; applied at the next turn; `auto_approve` see below), `chat.respond {session_id, need_id, response, fresh_stepup?}`, `chat.handoff {to: terminal | chat, session_id?, pane_id?, stop?}`, `chat.close`, `chat.file {session_id, path}` (below). Shapes: `runner/api/runner-api.schema.json`. A session summary (`chat.list`, `chat.get`) has `preview`, the masked first line of the last message (140 characters), and `previewBy: agent | you`, who wrote it; `previewBy` is left out when that is not known (a record from before it was kept).

**`fresh_stepup` is web's alone.** It says the person confirmed it's them in the last 5 minutes (a high-risk answer, Auto-approve). Web sets it on its REST twin from the session (never for an API token, never the caller's value); the gateway removes it from every client socket `req` (a socket's ticket proves 15 minutes at most).

**Events** (TypeScript: `packages/protocol/src/chat.ts`; Rust: `runner/crates/protocol/src/chat.rs`; both held to `fixtures/chat.json`). Every event has `seq`, `at`, `sessionId`, `type`, and optionally `turnId`, `agentPath` (a sub-agent's parent tool id) and `truncated` (a field was cut to fit 256 KiB).

| `type` | Fields |
|---|---|
| `session.opened` | `plugin`, `cwd`, `mode`, `login` (`api_key` · `cloud` · `subscription`), `model?`, `features[]`, `resumedFrom?` |
| `message.user` | `messageId`, `text`, `by?`, `clientMsgId?`, `queued?` |
| `message.delta` | `messageId`, `text` (ephemeral) |
| `message.done` | `messageId`, `text` (Markdown) |
| `thought.done` | `text` (empty when `redacted`), `durationMs?`, `redacted?` |
| `tool.started` | `toolId`, `kind` (read · edit · delete · move · search · execute · think · fetch · other), `title` (a one-line summary in the past tense), `input`, `locations[]`, `name?`, `server?`, `subject?`, `variant?` |
| `tool.updated` | `toolId`, `status`, `output?` (the latest tail: it replaces the previous one), `diff? {path, oldText, newText, line?}` |
| `tool.done` | `toolId`, `status` (completed · failed · denied · cancelled), `summary?`, `output?` (at most 16 KiB), `exitCode?`, `durationMs?`, `outputBytes?`, `outputKept?` (head · tail), `attachments?[{mime, bytes}]`, `resultCount?` |
| `plan.updated` | `entries[{text, status: pending · in_progress · done}]` |
| `needs.opened` | `item` (below) |
| `needs.resolved` | `id`, `outcome` (answered · expired · cancelled), `by?`, `response` |
| `files.changed` | `files[{path, change: created · modified · deleted, source: tool · watch, toolId?}]` (after the tool succeeded) |
| `turn.started` / `turn.done` | `turn.done`: `status` (completed · interrupted · failed), `error?`, `usage? {inputTokens, outputTokens, costUsd, durationMs, …}` |
| `usage` | `usage` |
| `session.mode` | `mode`, `model?` |
| `session.handoff` | `to` (terminal · chat), `paneId?` |
| `session.closed` | `reason` |
| `notice` | `code` (`unknown_event`, `agent_output`, `api_retry`, `output_lost`, `agent_exited`, `terminal_turns`, …), `text`, `native?`: a neutral row; the chat goes on |

**Tool calls and thoughts in detail** (docs/design/chat-tools.md §14; hello feature unchanged). Every field marked `?` below is **optional on the wire**: a runner from before it omits it, a reader from before it ignores it, and a reader that knows it must work without it (the fallback is in the last column). A reader refuses a value of the wrong type for a field it knows (a string for `exitCode`); it reads past a `variant`, `status` or `outputKept` value it does not know and past fields it has never heard of. `fixtures/chat.json` holds every event type both with and without these fields (`events`), the refused junk (`invalidOptional`) and what is read past (`tolerated`).

| Field | Meaning | Without it |
|---|---|---|
| `tool.started.name` | The tool's own name (`Bash`, `create_issue`; an MCP tool's name without its server), ≤ 64 characters | Show `title` |
| `tool.started.server` | The MCP server the tool belongs to, ≤ 64 characters | — |
| `tool.started.subject` | The object of the call: the command, the path, the pattern, the address, the helper's task. One line (a line break is sent as `⏎`), ≤ 300 characters, masked. The reader writes the verb, so it can say "Running" and then "Ran" | Show `title` (past tense even while running) |
| `tool.started.variant` | A refinement of `kind`, which keeps its nine values: `list` (read), `create` (edit: a write to a path that did not exist when the runner read the call; `files.changed` stays the authority on created or modified), `glob` (search), `web_search` (fetch), `mcp`, `agent` (a helper), `question` (other), `plan` (think). An unknown value is shown as the kind's default | The kind's default |
| `tool.updated.output` | Live output, for agents that stream it: **the latest tail, replacing the previous one**, never an increment to append | Output appears at `tool.done` |
| `tool.updated.diff.line` | The first line of the change in the file (1-based) | A diff without line numbers |
| `tool.done.status` | `denied`: a person or a rule said no and the call did not run. `cancelled`: the call was still open when its turn was interrupted; it comes before that turn's `turn.done`, under the same `agentPath` as its `tool.started`. An unknown status is drawn as finished | A runner from before sent `failed` for a refused call and nothing for an interrupted one: treat a call still running at an interrupted `turn.done` as stopped |
| `tool.done.exitCode` | A command's exit code, where the agent says (it may be negative) | No exit fact |
| `tool.done.durationMs` | The agent's own time for the call | The difference of the two events' `at` |
| `tool.done.outputBytes` | The size in bytes of the whole output the tool gave (before control sequences were removed, before the cut, before masking) | No size line |
| `tool.done.outputKept` | Present only when `output` is a cut of a longer one: `head` (reads, searches, fetches: the start matters) or `tail` (commands and everything else: the end matters). The cut end carries `…` | `output` is whole, or (an older runner) its tail |
| `tool.done.attachments` | What the result held besides text: `{mime, bytes}` per item, at most 20. **Never the bytes**: an image is not in the event, the log or a frame | The text `[image]` in `output` |
| `tool.done.resultCount` | Matches or files found, where the agent gives a count | No count |
| `thought.done.durationMs` | How long the agent thought (the block's start to its stop in the agent's stream) | "Thought" without a time |
| `thought.done.redacted` | `true`: the vendor hid this thought; `text` is empty | — |

**Control sequences.** Text in `tool.started`, `tool.updated`, `tool.done`, `thought.done`, `turn.done` and `notice` is chosen by programs and pages the agent touched. The runner removes terminal control sequences from every string of those events (CSI, OSC, DCS and the other escape sequences whole, and every control character except tab and newline; a carriage return becomes a line break) **before** masking, so a secret with a colour code in its middle is masked whole. An unclosed OSC or DCS ends at the end of its line. Characters that change the reading direction or have no width are kept: a reader shows them as visible marks. Readers must still treat this text as untrusted plain text (never Markdown, never markup, never a link).

**What the 30-day log keeps.** `events.ndjson` holds exactly the events above, as sent: the masked input (strings ≤ 4,096 characters), `subject`, the masked output cut (≤ 16 KiB), the masked diff (≤ 64 KiB a side), the facts in the table and masked thoughts. It never holds the full output, an attachment's bytes or anything unmasked.

**Needs-you items** (`answerVia: session`): a permission request (the plugin's held `PermissionRequest` hook), a question (Claude's `AskUserQuestion`, held at `PreToolUse`) or a plan (`ExitPlanMode`, a permission). `item`: `id`, `kind` (permission · question · plan), `risk` (normal · high), `options` (what may be answered), `title`, `tool?`, `input?`, `why?` (high risk, in plain words), `questions?[{text, header, options[{label, description, preview?}], multi}]`, `markdown?` (the plan), `expiresAt` (held 540 s). Answers (`chat.respond` `response`): `{kind: allow_once | allow_session | deny | answers | approve_plan | keep_planning, message?, answers?}`; `answers` maps a question's text (or its index) to a label, labels (multi-select) or free text ("Other…"); `deny` with a `message` is "tell it what to do instead" (for a question: skip with a reply). Outcome `accepted`, or `stale` when the item is gone (answered elsewhere, expired, interrupted, the runner restarted): exact, by id, never keystrokes. **High risk** (git push, package installs, `sudo`, deletes outside the project, network use, credential reads, edits outside the project): `allow_once` and `deny` only, and allowing needs `fresh_stepup: true`, which web sets itself (a session stepped up in the last 5 minutes; never an API token), else the runner answers `stepup_required`. Each item also goes to web as a durable machine event **`chat_needs`** `{sessionId, needId, kind, risk, title, tool, agent}` (paneId `""`) for push.

**Per login kind** (D-C4, decided 2026-10-10 "guarded (b)"; first written 2026-10-06). The plugin manifest's `[chat] logins` lists the login kinds that may chat (Claude: `api_key`, `cloud`; the login is read from the chat's environment: `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` = api_key, `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` = cloud, else subscription). A **subscription** login may chat only when web's policy for the machine's owner allows it: `config` carries `chat: {subscription: "allow" | "deny", autoApprove: "allow" | "deny"}` (an admin's Settings → Agent chat, over the server's `CHAT_SUBSCRIPTION`: `everyone` (default, interactive only) · `admins` · `nobody`, and `CHAT_AUTO_APPROVE`: `allow` (default) · `forbid`; unknown until the first `config` = deny). Otherwise `chat.start` answers `chat_login_not_allowed` and nothing starts: the terminal, with its permission buttons, is the way.

**Per transport** (Phase 21 C4). `[chat] transport` says how the runner drives the CLI; the events, the frames and the `chat.*` methods are the same for every one. `stream-json` (Claude): messages on stdin, events on stdout, decisions through held hooks. `json-rpc` (Codex's `app-server`): requests both ways on stdio, one JSON object per line. The runner opens a conversation (`initialize`, then `thread/start`, or `thread/resume` with the thread id it keeps as the chat's `vendorSessionId`), sends each message as `turn/start`, stops a turn with `turn/interrupt` (the process stays), and answers the agent's approval requests (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`) on the request's own id with `accept`, `acceptForSession`, `decline` or `cancel`: these are the needs-you items (`allow_once`, `allow_session`, `deny`), with the same risk rules and step-up. A line over 4 MiB or one that is not JSON is dropped with a notice; a request the chat cannot carry yet (a question, an MCP form) is refused on its id with the notice `answer_in_terminal`; a child that does not open its conversation in 30 s, or does not end an interrupted turn in 10 s, is stopped (`agent_silent`). `[chat] min_agent` is the oldest CLI version with the structured mode: on an older one `chat.start` fails with `chat_agent_too_old` (the message says which version is needed) and panes get no `Agent.chat`; a version that could not be read is not refused.

**Masking.** Before an event gets its seq, every string in it passes the runner's masker: the values of secret-named variables the chat's CLI can see (`*_API_KEY`, `*TOKEN*`, `*SECRET*`, …, at least 8 characters) and their base64 and percent-encoded forms, plus credential shapes (provider token prefixes, JWTs, private keys, `Authorization:` headers, `user:pass@` URLs, `NAME=value` where the name says secret). Long hex and base64 runs are kept (commit hashes). The log on disk, every frame, `chat.read`, `chat.list` and the `chat_needs` push event never hold a raw secret the runner knows of.

**Unattended runs on a subscription login.** `config.chat.unattended: "allow" | "deny"` (absent = allow, an older web) says whether the machine's owner may run a subscription login with nobody watching (web: admins allow, everyone else deny). With `deny`, a subscription chat in Auto-approve that nobody has watched (no `chat_attach`) for a minute goes back to `ask_first` (`session.mode` + `notice` `unattended_needs_api_key`), and `chat.mode {mode: auto_approve}` on such a chat answers `unattended_needs_api_key`. API-key and cloud logins are not affected. Headless, scheduled and triggered runs use the same switch when they land.

**Chat offer per pane** (hello feature `chat_offer`). A pane's `Agent` may carry `chat`: `{}` = this agent session can open as a chat (its plugin has `[chat]` and the login may chat), `{id}` = the chat that already holds the session (`chat.handoff {pane_id, to: chat}` continues that chat). The runner binds chats to panes from the plugin's `session.started` (session id, source) and `session.ended`: a chat whose session ends in its pane is released (`session.handoff {to: chat}` + `notice terminal_ended`; `terminal_closed` when the pane is gone, `terminal_new_session` when another session starts there); a chat whose session starts in a pane follows it (`session.handoff {to: terminal, paneId}`); if that chat was driving the session at the time, its agent stops (`notice opened_elsewhere`; the turn ends `interrupted`, waiting messages are dropped).

**Auto-approve** (Phase 21 "Bypass permissions", design agent-chat.md §11). `auto_approve` is an opt-in mode per chat: the plugin maps it to the CLI's own bypass mode (Claude `bypassPermissions`). `chat.start {mode: auto_approve}` and `chat.mode {mode: auto_approve}` need web's policy (`config` `chat.autoApprove: "allow"`; absent or `"deny"` answers `auto_approve_forbidden`) and `fresh_stepup: true` (else `stepup_required`). While it is on, the chat's generated hook settings add `tmuxedo-runner hook --guard` on **every** tool (`PreToolUse`, no matcher; hooks still run in bypass mode). The runner answers it: the high-risk classes above are the **never** list: denied, with the reason told to the agent, a `notice` `auto_blocked` in the chat and a durable machine event **`chat_auto`** `{sessionId, action: "blocked", tool, title, why, agent}`; anything else goes on (the CLI asks nobody) and is audited per turn as `chat_auto` `{action: "approved", count, tools[≤ 50], agent}` (and shown in the chat as its tool rows). The guard **fails closed**: no runner answer, junk or no chat id is a deny. A question (`AskUserQuestion`) is still held for a person. When web's policy turns to deny, every chat in Auto-approve goes back to `ask_first` (`session.mode` + `notice` `auto_approve_off`; a turn in progress has its remaining tools denied). A handoff to the terminal resumes without it.

**Files** (`chat.file {session_id, path}`, the Files panel's Preview · Raw · Changes). `path` is relative to the chat's work folder or absolute inside it. Confined by the runner's one path resolver (the jail, [design/file-browser.md](design/file-browser.md) §3.4): the path is walked from the work folder's open handle and **no link is followed**, in any position, whether it points outside or stays inside (`file_refused`); only regular files with one name (a hard-linked file is `file_refused`), on the work folder's own filesystem. A path is refused, not cleaned up, when it has a `..`, a `:`, a `\`, a control character, a component ending in a dot or a space, or a Windows device name (`con`, `nul.txt`, `aux.c`): `invalid_path` (`..`: `file_refused`, as before). **Secret files are refused**, by a name folded for case and Unicode (NFKC, zero-width characters removed, trailing dots and spaces removed) even when the agent wrote them (`.env*`, key files, SSH keys, `.netrc`/`.npmrc`/`.pypirc`/…, cloud and SSH credential folders, `.git/` internals; `packages/protocol/fixtures/secret-files.json` holds runner and web to one list): `file_refused`. Answer `{type: chat_file, path, absPath, mime, size, mtime, text?, data?, truncated, binary, changes}`: text up to 2 MiB, masked like every chat string; a PNG/JPEG/GIF/WebP up to 2 MiB as base64 `data`; anything else neither. `changes`: `{kind: new}` (created in this chat), `{kind: tool, diffs[{oldText, newText}]}` (its edit tools), `{kind: git, patch}` (`git diff HEAD` of a tracked file, masked, ≤ 512 KiB) or `{kind: none}`. Errors: `invalid_path`, `file_refused`, `file_not_found`, `file_unreadable`. **Turn-end scan:** at each `turn.done` the runner compares `git status` of the work folder with the one taken when the turn started; new or changed files that no tool event named (a shell command's output) become `files.changed` with `source: watch`. A folder that is not a git work tree is not scanned.

**Chat ⇄ terminal** (one driver per session). `chat.handoff {to: terminal}` waits for the turn boundary (`turn_running`, or `stop: true` to interrupt), stops the structured CLI and opens a new workspace whose pane runs the plugin's `[resume]` (`claude --resume <id>`); the chat says `session.handoff` and refuses messages (`chat_in_terminal`) until `chat.handoff {to: chat}`, allowed when the pane's agent is not working or blocked (`agent_busy`): the pane closes and the next message resumes the same session in structured mode (turns taken in the terminal are not in the chat's log; a `notice` says so). `chat.handoff {to: chat, pane_id}` with no `session_id` opens any pane's agent session as a new chat.

## Secrets (Phase 14)

Design: [design/secrets-and-env.md](design/secrets-and-env.md). Values never cross the relay in plain text: web seals each one to the
runner's own X25519 key (`packages/protocol/src/secrets.ts`, `runner/crates/runner/src/secrets/seal.rs`; fixtures
`packages/protocol/fixtures/secrets.json`, checked from both sides):

```
sealed = "x1." b64url(epk) "." b64url(nonce) "." b64url(ciphertext ‖ tag)
key    = HKDF-SHA256(salt = epk ‖ runner_pk, ikm = X25519(esk, runner_pk), info = "tmuxedo-secret-v1")
AEAD   = ChaCha20-Poly1305(key, nonce, aad = context)
```

`context` is `<machineId>|req:<id>` for the answer to request `id`, `<machineId>|mask` for a mask list: a blob replayed into
another answer or to another machine does not open.

| Frame | Direction | Fields | Meaning |
|---|---|---|---|
| `secret_key` | runner → gateway | `id`, `alg` (`x25519-hkdf-sha256-chacha20poly1305`), `publicKey` (base64url, 32 bytes) | Sent after every `welcome`. Web stores the key on the machine (audited when it changes) and answers `secret_res {id, result: {ok}}` followed by a `secrets` frame. The private key is a 0600 file in the runner's state directory. |
| `secret_req` | runner → gateway | `id`, `names` (1–32), `purpose` {`kind`: `exec`\|`mcp`\|`agent`\|`git`\|`pane`, `plugin`?, `paneId`?, `mcp`?, `program`? (resolved absolute path), `host`?, `project`?} | Values for one use. Web picks the narrowest scope per name (project > machine > user), checks its bindings allow this purpose, records the delivery (the mask list) and audits names and purpose. |
| `secret_res` | gateway → runner | `id`, `result` {`sealed`, `missing`[], `denied`[]} \| `error` {`code`, `message`} | `sealed` opens to `{values: {NAME: value}, versions: {NAME: n}}`. Codes: `not_allowed`, `secret_unavailable`, `secrets_unconfigured`, `no_key`, `unavailable` (web not reachable), `not_supported` (old gateway). |
| `secrets` | gateway → runner | `policy`? {`names`: [{`name`, `bindings`[]}], `offlineCache`, `guardOff`?[]}, `mask`? (sealed `{values: [{name, value}]}`), `revoked`?[] | After key registration and whenever web changes what the machine may use (`machine.secrets` RPC). `policy` holds names and bindings only. `mask` lists every value delivered to the machine in the last 30 days (every version), so the runner masks them in pane output even after a restart. `revoked` names are dropped from the runner's memory at once. |

Bindings (`@tmuxedo/protocol` `SecretBinding`): `{kind: "mcp", plugin, server}` (`server` `*` = any of the plugin's), `{kind: "git", hosts[]}`,
`{kind: "exec", programs[]}` (a bare name matches the resolved file name, an absolute path matches exactly), `{kind: "agent", plugin}`,
`{kind: "pane"}`. Both sides apply the same rule; web's answer is the authority.

**On the runner.** Values live in memory only, zeroized on drop, 15 minutes after their last use (pane-bound ones while the policy
binds them); an opt-in BYO machine keeps a copy sealed to its own key on disk. Every pane's bytes pass the masker at the holder read
(raw, base64 at any alignment, percent- and JSON-encoded; same length), and so do outgoing text frames and durable events. The local
socket (never the gateway) serves `secret.list`, `secret.git_credential {host}` and `secret.agent_env {plugin}` to processes in a pane
(`SO_PEERCRED` / `LOCAL_PEERPID` → session leader = a pane), and `secret-exec.sock` runs `tmuxedo secret exec` (the caller passes its
stdio with `SCM_RIGHTS`; the runner starts the allowed program itself). `project.env {cwd, previous}` answers the shell's cd hook.

**Events.** `secret_exposed` {`name`, `plugin`, `kind`, `transcript`} (durable, names only): a turn's transcript held a masked value.
Web marks the secret (the rotate banner) and pushes (`machine_health` group).

**Project env.** `config` also carries `projects` {project key: {NAME: value \| null}} (keys: normalised git remotes such as
`github.com/org/repo`, or `path:<machineId>:<abs>`). A new pane whose cwd is in a project gets that env over the machine's
(pane > project > machine > user; null unsets); bash and zsh panes get a cd hook in shell-init.sh that asks `project.env` on every
directory change and exports or restores accordingly.
## Queued prompts (Phase 26)

Runner hello feature: **`queued_prompts`**. Design: [design/away-from-desk.md](design/away-from-desk.md) D-AD6; web's routes: [api.md](api.md) "Queued prompts".

A prompt the owner types while the machine is offline is sealed by web to the runner's X25519 key (the Secrets mechanism above) under the context **`<machineId>|queued:<id>`**, so a blob does not open under another id or on another machine. The plaintext is `{v: 1, text, target: {chat: <sessionId>} | {pane: <paneId>}, expiresAt?}` (`expiresAt` in ms since the epoch); target and expiry live inside the seal, so web cannot redirect or extend a prompt. Web stores only the ciphertext.

When the machine is back (a machine-state post that does not say offline; at most every 10 s per machine) web calls, oldest first, **`queued.deliver {id, sealed}`** (`id` `[A-Za-z0-9_-]{8,64}`, `sealed` ≤ 96 KiB; web-only: not in the public method table). The runner opens it, then:

| Answer | Meaning | Web |
|---|---|---|
| `{type: "queued_delivered", id, to: chat\|pane, seq?, queued?, at}` | Delivered: a chat gets it as the owner's message (`client_msg_id` `q:<id>`; behind a running turn or an open decision it is queued as `chat.send` does), a pane has it typed with Enter | delivered |
| the same with `duplicate: true` | Already delivered (a resend after a lost answer or a restart): nothing is sent again | delivered |
| `session_ended` | The chat is gone, closed or in a terminal; the pane is gone or not running | failed |
| `queued_expired` | `expiresAt` passed before the machine was back | failed |
| `queued_unreadable` | It does not open (the runner key changed) or its plaintext is not the shape above | failed |
| `queued_busy` | The pane has an open decision or a held prompt: typing would answer it | still waiting, with that target's later prompts |

**Exactly once.** The runner keeps the last 512 delivered ids with their answers in `<state>/queued-delivered.json` (written atomically), so a resend answers `duplicate`; a chat also dedupes the `client_msg_id`.

## Checkpoints (Phase 23)

Runner hello feature: **`checkpoints`**. Design: [design/away-and-review.md](design/away-and-review.md) §3, §5; what is in and out: [handoff/checkpoints.md](handoff/checkpoints.md). Code: `runner/crates/runner/src/checkpoint/`, `runner/crates/protocol/src/checkpoint.rs`, `packages/protocol/src/checkpoint.ts`.

Before each agent turn (a hooked pane's `UserPromptSubmit`, a chat's turn) the runner records the session's work tree, when it is in a git repository, as a commit in its own bare repository (`$TMUXEDO_CHECKPOINTS_ROOT`, default `${XDG_DATA_HOME:-~/.local/share}/tmuxedo/checkpoints`; `off` turns checkpoints off), one per repository, under `refs/sessions/<session>/<n>`. It is built with a temporary index: the user's index, HEAD, branches, stash and `.git` are never written. Recorded: tracked files and untracked files that are not ignored, with their modes (executable bit, symlinks). Not recorded: ignored files, `node_modules`, `.venv`, `target`, `dist`, `.next`, secret files (the `chat.file` list), files over 50 MiB, nested repositories; a restore never touches them. A turn waits for its checkpoint at most 8 s (then the turn goes on and nothing is recorded); a turn whose files did not change since the last turn's checkpoint adds none. Kept: the last 50 per session, none older than 7 days.

**Methods** (`req`). A request names a session the runner knows (`pane` or `chat`, exactly one), never a folder. The machine's local socket answers every `checkpoint.*` with `not_supported`. A client socket may send `checkpoint.list`, `checkpoint.preview` and `checkpoint.create` with control of the owner's own machine (else `forbidden_scope`; a runner without the feature: `not_supported`); **`checkpoint.restore` changes files and travels through web only** (`POST /api/v1/machines/{id}/checkpoints/restore`, [api.md](api.md)); the gateway refuses it on a client socket with `forbidden_method`.

| Method | Params | Result `type` |
|---|---|---|
| `checkpoint.list` | `pane \| chat` | `checkpoint_list {available, reason?, message?, checkpoints[]}`, newest first. Not available (`available: false`, empty list) with an error code as `reason`, among them `not_a_repo` (the folder is in no git repository), `off`, `root_refused` (the checkpoints folder is inside this repository), `folder_gone`, `git_missing` |
| `checkpoint.create` | `pane \| chat` | `checkpoint_created {checkpoint \| null}`: a `manual` checkpoint now (also in a plain shell, which gets no turn checkpoints) |
| `checkpoint.preview` | `pane \| chat, id` | `checkpoint_preview {checkpoint, files[{path, change}], total, truncated, added, modified, removed}`: what a restore would do compared with now; `change` is `added` (the restore brings the file back), `modified` (contents or mode) or `removed` (the file is new since: the restore deletes it); at most 500 files listed |
| `checkpoint.restore` | `pane \| chat, id` | `checkpoint_restored {id, safety, added, modified, removed, conflicts[], conflict_count}`: first records a `restore` checkpoint of now (`safety`, its id: restoring it undoes the restore), then writes the files back byte for byte with their modes and deletes the files that are new since. A path that cannot be written (a folder in the way, a parent that is a link) is a conflict, left as it is |

A checkpoint is `{id, kind, at, files, skipped: {large[], large_count, secret, other}, restoring?}`; `kind` is `turn`, `restore` or `manual`; `at` ms since the epoch; `restoring` (a `restore` checkpoint) the id it was taken before. **Errors:** `invalid_params`, `not_found` (no such session or checkpoint), `session_busy` (restore while the pane's agent is working or a chat turn runs: stop it or wait), `not_a_repo`, `off`, `root_refused`, `too_many_files` (over 200 000), `git_missing`, `git_failed`, `git_timeout`.

Only files: the conversation, and anything the agent's commands did outside the work tree, stay as they are.

## Headless tasks (`task.*`, Phase 18)

Web starts these for a service account (docs/api.md "Headless tasks"); a client socket may not start or cancel them (the gateway answers `forbidden_method`), and observe-only sockets may read them. The runner's hello carries `automation: {labels}` when its owner turned automation on (`--automation-label`, `automation_labels` in `runner.toml`; up to 16, `[a-z0-9][a-z0-9-]{0,31}`); web stores them as `Machine.automationLabels`.

| Method | Params | Result |
|---|---|---|
| `task.start` | `task_id` (`tk_` + 16), `kind: command\|agent`, `labels` (the policy's), `cwd`, `roots`, `argv` + `allowed_commands` (command), `agent` + `allowed_agents` + `mode` + `text` (agent), `label?`, `timeout_ms?` (≤ 24 h, default 1 h) | `task_info {task}` |
| `task.get` | `task_id` | `task_info {task}` |
| `task.list` | — | `task_list {tasks}` |
| `task.cancel` | `task_id` | `task_info {task}` |

`task`: `{task_id, kind, label, cwd, status: running|done|failed|cancelled|timed_out, reason_code: exit_status|spawn_failed|timeout|permission_needed|taken_over|closed|lost|null, reason, exit_code, signal, output, output_truncated, pane_id, session_id, started_at, ended_at}`. The runner checks again: a label it carries, the resolved folder under a root, the resolved program among the allowed ones (no shell), the agent and a mode that is never auto-approve, observe-only and its own limit (32 running). Refusals: `automation_disabled`, `automation_not_allowed`, `observe_only`, `cwd_not_allowed`, `command_not_allowed`, `agent_not_allowed`, `mode_not_allowed`, `limit_reached`, `task_exists`, `task_not_found`. Tasks live in the runner's memory; after a restart `task.get` answers `task_not_found` and web marks the task `failed` / `lost`. Types: `packages/protocol/src/tasks.ts`; schema: `runner/api/runner-api.schema.json`.

## Files

A folder per project on the machine, **outside every repository**, where files a person sends arrive (`inbox/`) and where an agent leaves files for them (`outbox/`). Design: [design/files-and-uploads.md](design/files-and-uploads.md). Runner hello feature: **`files`**. Code: `runner/crates/runner/src/files/` (store, worker, glue), `runner/crates/protocol/src/files.rs`, `packages/protocol/src/files.ts` and `file-ticket.ts`, `gateway/src/files.ts`, `web/src/server/files.ts`, `web/src/lib/uploader.ts`. Shared rules (file names, chunks, ranges, disk levels) and frames are held to `packages/protocol/fixtures/files.json` from both sides.

**Layout.** `$TMUXEDO_FILES_ROOT` (default `${XDG_DATA_HOME:-~/.local/share}/tmuxedo/files`, 0700; `off` turns files off) → `<project>/inbox/<stamp>_<name>` (0444, never executable, with a provenance sidecar `inbox/.meta/<name>.json`: `{uploadedBy, at, source, originalName, sha256, size, untrusted: true, pinned}`, and the xattr `user.tmuxedo.provenance` where supported) and `<project>/outbox/<name>`; uploads in progress in `.partial/<uploadId>/{data, state.json}`. A project is the normalised git remote (`github.com_org_repo`), else the repository's or folder's name with a hash of its path (`path_notes_1a2b3c4d`). A root inside a git work tree (also through a symlink) turns files off at start, logged, so an upload can never be committed by accident. Every pane gets `TMUXEDO_FILES_ROOT` and `TMUXEDO_FILES_DIR` (its project's folder, made at pane start). `.instructions.md` in the root is what agent CLIs are handed (Claude: `--append-system-prompt-file`) together with `--add-dir $TMUXEDO_FILES_DIR`, both only in a pane with a files folder (the plugin manifest's `launch.files`).

**FILE frame** (binary kind **5**), the pane-frame header reused: `kind 5 · flags 0 · req = the chunk number (upload) or read number (download) · id = the upload id or download transfer id ([A-Za-z0-9_-]{8,64}) · offset = byte offset in the file`; at most 512 KiB of payload. The gateway routes on the header and never reads the bytes.

| Frame | Direction | Meaning |
|---|---|---|
| `file_put {uploadId, n, size, sha256}` then FILE frames | gateway → runner | chunk `n` of `size` bytes follows; FILE frames in order, at offsets `n × chunkSize …`. The runner writes them with `pwrite` while hashing; only to runners with the `files` feature |
| `file_put_cancel {uploadId, n}` | gateway → runner | the device went away mid-chunk: forget what arrived of it |
| `file_ack {uploadId, n, ok, sha256? \| code, message}` | runner → gateway | the chunk is on disk (fdatasync'd, recorded in `state.json`) with this SHA-256, or why not: `chunk_hash_mismatch`, `chunk_out_of_order`, `invalid_chunk`, `upload_not_found`, `upload_complete`, `insufficient_storage` (the disk filled: the upload is stopped and removed) |
| `disk {freeBytes, totalBytes, filesBytes, level}` | runner → gateway → clients (and machine-list sockets) | the home disk: after each connect and when the level or the numbers move (checked every 60 s). `level`: `ok`, `low` (< 10 % free: banner, uploads refused), `critical` (< 5 %: pane history stops, a durable `disk_low` event), `alert` (< 2 %: the operator's alert, `tmuxedo_machine_disk_free_ratio`) |

The runner answers the bytes on its own thread: upload frames go from the socket reader to it through a bounded queue, so a slow disk slows the socket and the gateway, which stops reading the device's request while the runner socket holds more than 2 MiB (backpressure end to end; the gateway holds at most one 512 KiB frame per request and one 4 MiB window per download).

**Methods** (`req`; `files.share` and `files.request` on the machine's local socket only, for the agent; `files.upload_*` and `files.read` from web and the gateway only: the gateway refuses them on client sockets with `forbidden_method`, and the local socket with `not_supported`):

| Method | Params | Result `type` |
|---|---|---|
| `files.list` | `project` or `pane_id` or `session_id` (local: `cwd`), `dir?` | `files_list {project, path, files[{name, dir, path, size, mtime, pinned, expiresAt?, source?, sha256?, note?, originalName?}]}`, newest first |
| `files.stat` / `files.delete` / `files.pin` | `project, dir, name` (+ `pinned`) | `files_stat` (an entry) / `files_deleted` / `files_pinned` |
| `files.quota` | | `files_quota {usedBytes, quotaBytes, maxFileBytes, freeBytes, totalBytes, floorBytes, level, uploadsAllowed}` |
| `files.share` | `path, note?` (+ `cwd`, `pane_id`) | `files_shared {project, name, path, size, sha256}`; copies into the outbox (a second name, never an overwrite) and raises the durable event `file_shared`. Only from the session's work folder (its repository) or the files root, never a secret file (`.env*`, keys, credentials: the list of `chat.file`), never through a link and never a file with a second name (a hard link) (`file_refused`). Opened by the same resolver as `chat.file` (the jail): `path` is matched to one of those folders by name (the folder's path as the runner knows it, or its real path), a `..` in it is `invalid_params`, and no link under the folder is followed. The `tmuxedo files share` command sends the real path, so sharing through a link still works when what it points to is in one of those folders |
| `files.request` | `what` (+ `cwd`, `pane_id`) | `files_requested {project, requestId, inbox}`; the durable event `file_requested {project, what, requestId, itemId?}`, and a needs-you `file` item ("Needs you" below: the item, not the event, is pushed; `itemId` says so) |
| `files.upload_init` | `upload_id, project \| pane_id \| session_id, name, size, chunk_size, chunk_hashes?, request_id?, sha256?, by, source` (`request_id`: the file request it answers; `file_uploaded` carries it as `requestId`. `sha256`: the whole file's hash, given up front; the runner then completes the upload itself as soon as its last chunk lands, a mismatch aborting it: the phone's background upload has no code left running to call complete) | `upload_init {uploadId, project, name, size, chunkSize, chunkCount, received[], expiresAt}`; refusals `quota_exceeded`, `insufficient_storage`, `file_too_large`, `upload_conflict` |
| `files.upload_status` | `upload_id` | `upload_status {…, received[], expiresAt, done?}` (resume) |
| `files.upload_complete` | `upload_id, sha256?` | `upload_complete {path, name, project, sha256, size}`: every chunk present (`incomplete` otherwise), the whole file hashed from disk and compared (`file_hash_mismatch` discards it), renamed into the inbox, sidecar written, durable event `file_uploaded {project, name, path, size, sha256, source, paneId, requestId, sessionId, by}` (the pane's event when the upload named a pane). Asking twice answers twice |
| `files.upload_abort` | `upload_id` | `upload_aborted`, always |
| `files.read` | `xfer, n, project, dir, name, offset, length ≤ 4 MiB` | FILE frames (id `xfer`, req `n`) with the bytes, then `files_read {size, offset, bytes, eof, mtime}` |

**Limits** (environment overrides for operators and tests: `TMUXEDO_FILES_MAX_FILE`, `TMUXEDO_FILES_QUOTA`, `TMUXEDO_FILES_MIN_FREE`; `TMUXEDO_DISK_FAKE=<free>,<total>` for tests only): largest file 2 GiB (≤ 10 GiB); files on a machine at most 25 % of its disk, ≤ 20 GiB (hosted: 5 GiB of 20); an upload must leave max(2 GiB, 10 %) free, checked at init and before every chunk; partial uploads go 24 h after their last chunk (hourly sweep), inbox and outbox files after 14 days unless pinned (daily sweep, inside the root only, never through a link).

**Tickets** (`file-ticket.ts`): `f1.<claims>.<hmac>` with the attach tickets' secret and their own version tag and audience (`tmuxedo-files`), so neither verifier accepts the other's. Upload: `{op: upload, sub, sid, mid, uid, size, cs}`, 15 minutes; download: `{op: download, sub, sid, mid, project, dir, name}`, 5 minutes, presented as `Authorization: Ticket …` or `?t=` on a plain link. The gateway serves `/gw/files/*` only for the ticket's own user's machine with the `files` feature: `PUT {uploadId}/chunks/{n}`, `GET {uploadId}`, `POST {uploadId}/complete`, `DELETE {uploadId}`, `GET download[/{name}]` (with `Range`: one range, 206, 416 when unsatisfiable). docs/api.md "Files" has the web routes that mint them.

**Untrusted.** An upload is data from the person, never instructions: the instructions file and the generated skill say so; nothing is sent to an agent by itself ("Insert path" types `@<path>` (Claude, Cursor) or the plain path (Codex) without Enter); and a `Read` of an inbox file by a hooked agent marks the pane's turn, so a permission it asks for next carries `prompt.afterUpload` (the file's name), shown on the needs-you card and in its push ("After reading …").

**Old runners** list no `files` feature: the gateway answers `/gw/files` with 501 `not_supported` and web says "update the runner".

## Close codes

| Code | Meaning |
|---|---|
| 1001 | Gateway or runner shutting down; reconnect. |
| 1008 | Runner: no `hello` within the deadline, or a first frame that is not `hello`. |
| 1009 | A frame over the socket's size limit (client 1 MiB, runner 16 MiB). |
| 4001 | `replaced` — a newer runner socket for this machine. Do not reconnect from the old process. |
| 4002 | `unsupported_version`. |
| 4003 | Do not reconnect. Runner: `revoked` (its token was revoked; `machine.revoke`), `account_deleted` (its owner deleted their account: remove the runner with `tmuxedo-runner uninstall-service`; queue 3e) or `proof_failed` (a key-bound token whose proof was missing, wrong or late; see "Runner proof"). Client: `revoked` (its session or API token was signed out, revoked or reset; `client.revoke`) or `forbidden` (the ticket names a machine another user's runner now serves). |

A machine whose runner leaves does not close its clients' sockets: they get `machine {online: false}` (and `error` `machine_offline` for frames that need the runner) and resume when it returns.

## Runner enrolment (device flow, `kind: "runner"`)

The runner never sees a user's session and never invents its own identity: web mints the runner token server-side when a signed-in, **passkey-stepped-up** user approves, because approving grants a shell on that machine to whoever holds the token.

1. `POST /api/device/start` `{"kind":"runner","deviceName":"<hostname>","os":"linux|macos","arch":"x86_64|aarch64"}` →
   `200 {"deviceCode","userCode","verificationUri","verificationUriComplete","interval":5,"expiresIn":900}`
2. The runner prints `verificationUriComplete` (and `userCode`). The user opens it, signs in if needed, is asked for a passkey step-up, sees "Allow *deviceName* (os/arch) to run terminals as you?", and approves or denies.
3. `POST /api/device/poll` `{"deviceCode"}` every `interval` seconds →
   - `428 {"error":"authorization_pending"}` keep polling; `429 {"error":"slow_down"}` add 5 s;
   - `403 {"error":"access_denied"}` / `410 {"error":"expired_token"}` stop;
   - `200 {"kind":"runner","runnerToken":"tmr.<lookup>.<secret>","machineId","server":"https://tmuxedo.dev"}` exactly once; a second poll after success returns `410`.
4. The runner stores `server` and `runnerToken` in `~/.config/tmuxedo/runner.toml` (mode 0600) and connects to `/gw/runner`.

On approval web creates the `Machine` (`kind: byo`, name = deviceName, unique per user with a numeric suffix if needed) and a `RunnerToken` (lookup + SHA-256 of the secret only). Revoking the token in the UI closes the runner socket with 4003.

## Machine pairing (`/api/pair/*`)

The QR / match-code / approve flow used by tmuxedo for Mac and `tmuxedo-runner pair`; full contract and threat
model in [design/mac-pairing.md](design/mac-pairing.md). The device flow above is unchanged and stays supported.
A pairing started with a public key produces a **key-bound** token (below); a keyless CLI pairing produces a
bearer-only token like the device flow.

## Runner proof (key-bound tokens)

A `RunnerToken` may carry a P-256 public key (`publicKey`, `keyFingerprint`). Such a token is useless without the
private key:

1. The runner connects with `Authorization: Bearer tmr.<lookup>.<secret>` and sends `hello` as usual.
2. Web's `runner-auth` answer carries `proofKey` (the JWK; `null` for a bearer-only token). For a key-bound token
   the gateway sends **`{t:"challenge", nonce}`** (32 random bytes, base64url) at once and holds the `hello`.
3. The runner answers **`{t:"proof", sig}`**: ECDSA P-256 / SHA-256, base64url DER or raw r‖s, over
   `tmuxedo-runner-proof-v1\n<nonce>\n<lookup>`. On a Mac the signature comes from the app's Secure Enclave key
   through the signer fd (`TMUXEDO_SIGNER_FD`, design/mac-pairing.md §3.2).
4. The gateway verifies it. Valid → the held `hello` is processed and `welcome` follows. Anything else before the
   proof, a wrong signature, or no proof within 10 s → close **4003 `proof_failed`**, counted in
   `tmuxedo_gateway_runner_proof_total{outcome}` and audited by web as `runner.proof_failed` (IP and reason only,
   via `/api/internal/terminal-audit`).

A malformed `proofKey` from web refuses the socket (503), never degrades to bearer. Bearer-only BYO tokens can be
refused altogether with web's `RUNNER_BEARER_TOKENS=deny` (hosted machines are exempt). Golden fixture:
`packages/protocol/fixtures/runner-proof.json` (TypeScript `test/runner-proof.test.ts`, Rust `gateway.rs`).

## Demo machines (App Store review accounts)

An App Store review account (queue 3d, `web/src/server/review/`) owns one machine of kind `demo`, `tmuxedo-demo`, that
no runner, pod or shell backs. The gateway plays its runner side itself (`gateway/src/demo.ts`). A `DemoRunner` is a
`RunnerConn` whose socket lives in-process: the Hub sends it the frames a runner gets and it answers with ordinary
runner frames. Clients, web's projection, needs-you items and push therefore work unchanged. Code:
`packages/protocol/src/demo.ts` (identity and topology, shared with web), `gateway/src/demo.ts`.

- **Content.**
  - Workspace `acme-api`. Tab `main` is split horizontally: shell `w1:p1` and agent `w1:p2`. Tab `notes` holds one more
    shell, `w1:p3`.
  - The shells edit a line and answer canned commands (`help` lists them: `ls`, `pwd`, `whoami`, `date`, `echo`, a few
    version-control commands, `cat README.md`, `claude --version`, `clear`). Anything else prints
    `demo: <cmd>: nothing runs on this demo machine`.
  - The agent pane loops: working lines, then a permission prompt (`status` blocked with `promptId`/`prompt`, plus a
    durable `event` `blocked`, which makes a real needs-you item and push), then approve or deny (by `decision`, or by
    typing `1`/`y` or `2`/`n` in the pane), then `done` (event), a pause, and the next task.
  - Stale decisions get `decision_stale`. Read methods answer: `pane.list`, `pane.get`, `pane.read`, `agent.read`,
    `workspace.list`, `tab.list`, `agent.list`. `pane.send_text` and `pane.send_keys` drive the script. Every other
    method gets `not_supported`.
  - Glances, replay from an offset and snapshots work.
  - **Nothing executes anywhere.**
- **Who wakes it.** Only web vouches that a machine is simulated: a signed ticket claim `demo: [machineId]` (attach
  tickets for a `demo` machine; the dashboard's machine-list ticket of a review account), or `demo: {userId}` on the
  `machine.decision` and `machine.call` RPCs, or `demo.touch {machineId, userId}` (sent when the user loads the machine
  list or "Needs you"). The simulator never takes over a machine that has a real runner connected, and never
  runs one machine for another user.
- **Cost and pause.** There is no process per account, only one small object per running machine. A simulator runs
  while a client is attached or glancing, or for 2 minutes after a touch or RPC. Otherwise it pauses, which is NOT
  reported as `online: false` (web lists `demo` machines as online). A small state object (script position, held
  prompt) is kept for up to 7 days, so a prompt raised before a pause can still be approved from a notification.
  `machine.revoke` drops it.
- **Event seqs** are `max(previous + 1, now in ms)`, renumbered on `events_rejected` like a runner's.

## Internal endpoints (cluster-only, bearer `GATEWAY_RPC_SECRET` unless noted)

| Endpoint | Caller | Purpose |
|---|---|---|
| `POST web:3000/api/internal/runner-auth` `{token, ip?}` | gateway | → `200 {machineId,userId,lastEventSeq,config}` or `401`. `config` is the machine's effective launch / shell configuration (below), sent to the runner right after `welcome`. `lastEventSeq` (highest stored event seq, 0 if none) goes into the runner's `welcome`; while an answer is cached the gateway raises it as web confirms stored events. Updates `lastUsedAt` (and `lastUsedIp` from `ip`). The gateway caches a 200 for 60 s and a 401 for at most 5 s; web unreachable → the upgrade gets 503 so the runner retries. |
| `POST web:3000/api/internal/machine-state` `{machineId, online, hello?, topology?, agents?}` | gateway | Projection of machine/topology into Postgres. `agents` = latest `status` per pane since the topology (`{paneId: Agent \| null}`), applied after `topology`. A topology whose `rev` is lower than the one projected is stale and ignored (equal = harmless resend); a post with `hello` resets that baseline, so a runner that lost its state may start again at rev 1. |
| `POST web:3000/api/internal/agent-events` `{machineId, events:[…]}` | gateway | Durable events (idempotent on `(machineId, seq)`). Each stored `blocked`, `done`, `auth_failed`, `memory_stopped` or `resume_refused` is queued for Web Push, never awaited by the ingest (see "Web Push" below). → `200 {ok, inserted, rejected}`: `rejected` lists seqs that already hold a DIFFERENT event (type, pane, `at` or data differ: a runner reused numbers). Those are not stored; the gateway logs and counts them (`tmuxedo_gateway_events_rejected_total`, web `tmuxedo_agent_event_seq_collisions_total`) and never acks them, so the runner keeps them and renumbers them after its next `welcome`. A resent identical event is a duplicate, not rejected, and is acked. |
| `POST gateway:8081/internal/rpc` `{method, params}` | web | `machine.call {machineId, method, params, timeoutMs?}` → `{result}` (409 `machine_offline`, 504 `timeout`, 502 runner error; `timeoutMs` 100–130 000 overrides `GATEWAY_REQ_TIMEOUT_MS` for long-polls such as the public API's `agent.wait`), `machine.online {machineIds}` → `{result: {online: {id: bool}}}`, `machine.revoke {machineId, reason?: revoked\|account_deleted}` → `{result: {closed}}` (closes the runner 4003 with that reason, default `revoked`, and drops the cached admission; any other reason is 400), `machine.config {machineId, config}` → `{result: {delivered}}` (web changed the machine's configuration: launch / shell settings and the plugin set; an object of at most 960 KiB, sent to the runner now if it is connected, and remembered in cached admissions for its next connection), `machine.secrets {machineId, frame}` → `{result: {delivered}}` (a `secrets` frame for a connected runner; see "Secrets"), `client.revoke {sid | sids[] | userId}` → `{result: {closed}}` (closes, 4003, every client socket whose ticket `sid` is one of those session / API-token ids, or every client socket of that user; web calls it on sign-out, session revoke, sign-out-everywhere-else, token revoke and admin MFA reset), `machine.decision {machineId, paneId, promptId, behavior: allow\|deny, message?}` → `{result: {outcome: accepted\|stale\|unknown}}` (409 `machine_offline`; see "Prompt decisions"), `machine.login_callback {machineId, paneId, url}` → `{result: {ok, status, excerpt?}}` (422 with the runner's refusal code, 409 `machine_offline`, 501 `not_supported`, 504 `timeout`; see "Login callback relay"), `demo.touch {machineId, userId}` → `{result: {running}}` (keep a demo machine's simulator running a while), `ping`. `machine.call` and `machine.decision` also take `demo: {userId}` for a demo machine, and `machine.revoke` on a demo machine drops its simulator (see "Demo machines"). |
| `POST web:3000/api/internal/secrets` `{machineId, frame}` | gateway | A runner's `secret_key` or `secret_req` (see "Secrets") → `200 {result?, error?, frame?}`; values only ever sealed to that runner. |
| `POST web:3000/api/internal/terminal-audit` `{action: "terminal.login_callback", userId, machineId, sid?, meta: {via: "socket", paneId, host, outcome, status?}}` | gateway | An audit row for a `login_callback` a client socket sent (web never saw it). Only that action and those fields (others are dropped); only for a machine the user owns. → `200 {ok}` or `422` (not retried). Queued and retried like the projection. |
| `GET web:3000/api/internal/hosted-machines` (bearer `PROVISIONER_SECRET`) | provisioner, every 10 s | → `{machines: [{machineId, userId, desiredState: running\|stopped\|deleted, imageTag, pvcSizeGi, memoryRequestMi, memoryLimitMi, runnerToken?, deleteHome?}]}`. `runnerToken` (plaintext `tmr.…` (`amr.…` for one minted before the rename), minted by web when the hosted machine is created) is sent only until the provisioner reports `tokenStored: true`. A machine stays listed as `deleted` until the provisioner reports `deleted: true`. Full contract: `provisioner/CONTRACT.md`. |
| `POST web:3000/api/internal/hosted-machines/status` `{machineId, observedState: pending\|starting\|running\|stopped\|error, message, tokenStored, deleted?}` (bearer `PROVISIONER_SECRET`) | provisioner | Observed state on change and every 10 min. `error` + `cluster full — enrol your own machine instead` when the hosted cap is reached. |

All `/api/internal/*` routes answer 404 to anything without the bearer, and 404 on the public host even with it (web `proxy.ts` and the handlers both check). The gateway reaches web by its Service name (`WEB_INTERNAL_URL`).

**Token handover.** The runner token is minted on the first poll after approval, inside the transaction that marks the device code delivered, and exists in plaintext only in that response; web stores its lookup and the SHA-256 of its secret. The device code itself is stored only as a peppered digest.

## Prompt decisions (Phase 6)

A held Claude permission prompt is answered through the hook, bound to its `promptId`, never by keystrokes. Two paths reach the runner's `decision` frame:

- a client socket with `control` scope sends `decision` itself (the TUI does);
- web's `POST /api/agents/decision {machineId, paneId, promptId, behavior, message?}` (phone, notification action, "Needs you") calls the gateway RPC `machine.decision`. Web first requires a session stepped up within 15 minutes, or a `terminal` token, and ownership of the machine; every call is audited (the reply text is never stored, only its length).

The runner has no `res` for a decision, so the gateway derives the outcome from what follows on the runner socket, which is ordered:

| The runner shows | Outcome |
|---|---|
| a `status` for the pane whose `agent.promptId` is no longer the decided one, and no `decision_stale` for it within 400 ms | `accepted` (web 200) |
| `event` `decision_stale {promptId}` | `stale` (web 409 `prompt_stale`: "already answered or expired") |
| — the gateway already knows from `status`/`topology` that the pane holds a different prompt or none | `stale`, without asking the runner |
| nothing within 5 s | `unknown` (delivered, unconfirmed; web 202) |

**Expiry.** The runner owns the 540 s deadline. When it passes, the runner answers the hook `{expired: true}` (the hook prints nothing, so Claude's own dialog stays on the terminal), sends a `status` without `promptId` (the agent stays `blocked`), and a durable `event` `prompt_expired {promptId}`. A decision for that id afterwards gets `decision_stale`, never `accepted`. The hook itself waits 550 s, so it never gives up before the runner.

A notification action never decides by itself: the service worker records the intent (same-origin Cache Storage, one-shot, 2-minute life) and opens the app, which makes the same `POST /api/agents/decision` and runs a passkey/TOTP step-up first when the session's is stale.

## Web Push (Phase 6)

- VAPID (RFC 8292) keys are server configuration: `VAPID_PUBLIC_KEY` (uncompressed P-256 point, base64url), `VAPID_PRIVATE_KEY` (32-byte scalar, base64url) and `VAPID_SUBJECT` (a `mailto:` contact). Bodies are RFC 8291 `aes128gcm`, one record (`web/src/server/push/webpush.ts`, pinned to RFC 8291 Appendix A).
- `PushSubscription` rows come from `POST /api/push/subscribe` (MFA-verified browser session only; public https push services only) and are tied to the subscribing session: signing it out stops its notifications. `GET /api/push`, `POST /api/push/unsubscribe`, `POST /api/push/test` complete the set.
- The ingest enqueues; a bounded in-process worker claims `AgentEvent.notifiedAt` (the dedupe per `(machineId, seq)`), applies the user's per-type preferences and quiet hours, drops events older than 15 minutes, and sends. 404/410 disables the subscription; 429/5xx/network retry with back-off (Retry-After honoured) up to 4 attempts and pause that push service; other 4xx count a failure (10 in a row disables).
- Payloads carry the machine name, pane, agent kind, a masked one-line preview of the tool input (`web/src/lib/secret-mask.ts`) and a same-origin deep link; `blocked` adds `promptId` and Approve/Deny actions. TTL 540 s for prompts (the runner holds them that long); urgency high for prompts and auth failures.

## File browser (`fs.*`)

Runner hello feature **`fs`** (design: [design/file-browser.md](design/file-browser.md)). The reading methods are built; the
ones that change files are not. Shapes: `runner/api/runner-api.schema.json`; names, limits, the path rule, cursors, parameter
shapes and versions: `packages/protocol/src/fs.ts` and `runner/crates/protocol/src/fs.rs`, both held to
`packages/protocol/fixtures/fs.json`.

| Method | Params | Result |
|---|---|---|
| `fs.roots` | | `fs_roots {roots[{id, kind: workspace\|project, label, writable, place?: {root, path}}], refused[{label, kind, why: too_broad\|unreachable\|gone}], canWrite, why?, limits}` |
| `fs.list` | `root, path ("" = the root), cursor?, limit? (1–500)` | `fs_list {root, path, entries[{name, kind: file\|dir\|link\|other, size, mtimeMs, mime?, ignored?}], hidden, skipped, partial, next?}` |
| `fs.stat` | `root, path` | `fs_stat {entry, version?, editable, why?}` |
| `fs.read` | `root, path` | `fs_file {root, path, name, mime, size, mtimeMs, version?, kind: text\|image\|binary, text?, truncated, masked, eol, bom, finalNewline, editable, why?}` |
| `fs.search` | `root, query (1–100 characters), limit? (1–50)` | `fs_found {matches[{path, kind}], partial, hidden}` |

- **A path** is relative to a root and `/`-separated. Refused (`invalid_path`), never cleaned: empty (except `fs.list`'s root),
  an empty component, `.` or `..`, a leading or trailing `/`, `\`, `:`, a control character, a component ending in a dot or a
  space, a Windows device name, more than 4096 bytes, 64 components or 255 bytes a component. A **root** is an id from
  `fs.roots`; nothing else names a folder.
- **The runner opens every path from the root's own folder handle and follows no link.** A link is listed as `kind: link`
  without its target and cannot be opened (`symlink_refused`); a file with several names is `linked_file`; a secret-named
  entry is counted in `hidden` and never named, and asking for it is `file_refused`, as is any file holding a private-key
  block. A name that is not valid UTF-8 is counted in `skipped`.
- **`version`** is `v1.<sha256>.<size>.<mtimeNs>`; the hash decides. Only a file read to its end has one (up to 2 MiB).
- **Text** is masked like every chat string (`masked: true`) and cut so the answer fits one frame (`truncated: true`; about
  880 KiB of JSON). An image or a PDF is described, never inlined. `editable` is false with `why` (`binary`, `not_utf8`,
  `too_large`, `truncated`, `masked`, `read_only_root`); no root is writable yet.
- **Who.** Never on the machine's local socket (`not_supported`: anything in a pane can reach it). On a client socket the
  gateway relays the reading methods to a socket of the machine's owner that has control **or** whose ticket carries the
  signed `browse` claim (`forbidden_scope` otherwise), 20 a second with bursts of 60 per socket (`rate_limited`), and
  refuses every other `fs.*` name (`forbidden_method`); a runner without the feature is `not_supported`. Web sets `browse`
  on a machine ticket for the owner's own session and for an API token with `read` and `terminal`; never on a glance
  ticket. A ticket without the claim (older web) browses with control only. REST twin: docs/api.md "File browser".
- **`fs.download`** `{root, path, xfer, n?, offset?, length? (≤ 4 MiB)}` is the gateway's own (for `GET /gw/files/fs/…`
  with a one-time `fs_download` file ticket naming `{machine, root, path, user}`, a minute long): one window of the file as
  FILE frames, then `fs_download {name, size, offset, bytes, eof}`; `length: 0` is the size and the checks only. Every
  call opens the path through the jail again; key material is `file_refused`, and text the masker would change (in the
  first 2 MiB or in the window) is `masked_file`: a download is the bytes as they are, so such a file is not offered. A
  client socket never reaches it (not a reading method there).
- **Errors:** `invalid_params`, `invalid_path`, `root_unknown`, `root_gone`, `outside_root`, `symlink_refused`,
  `linked_file`, `file_refused`, `masked_file` (download), `not_found`, `not_a_file`, `not_a_dir`, `permission_denied`,
  `rate_limited`, `not_supported`. A message never names a path outside the root.
- **Changing files (slice A2):** `fs.write {root, path, text (≤ 512 KiB, no NUL), base | create: true}`,
  `fs.mkdir {root, path}`, `fs.rename {root, from, to, base?}` (within one root, never replaces), `fs.delete {root, path,
  base}` (a file; an empty folder takes no `base`) and `fs.undo {undoId}`, each with `op_id` (a retry gets the first
  answer) and web's flags `control: true`, `stepup_at` (ms; at most 15 minutes old, 5 minutes of clock skew allowed) and
  `by`. Results: `fs_written {path, version, size, mtimeMs, undoId}`, `fs_made {path, undoId}`, `fs_renamed {path,
  version?, undoId}`, `fs_deleted {path, undoId}`, `fs_undone {path, undoId, version?}`. Only `workspace` and `project`
  roots are writable (`fs_roots` says `writable` per root and `canWrite: false, why: observe_only` on an observe-only
  machine). They reach the runner **only** through web's internal RPC `machine.fs_write {machineId, method, params}`:
  the gateway checks the flags again, 60 a minute per machine, never logs a path or text, and refuses them on client
  sockets (`forbidden_method`, flags stripped from any `fs.*` a socket sends) and through `machine.call`. The runner
  refuses a change without the flags (`control_required`, `stepup_required`), on a read-only root (`read_only`), in
  observe-only mode (`observe_only`), on a secret name on either side, through a link, on a hard-linked file, past
  512 KiB, on key material or masked text (`masked_file`); a stale `base` is `conflict` (re-`fs.stat` for the current
  version). A replace is a same-folder temporary file, `fsync`, `RENAME_EXCHANGE`, then a check of what came out. What a
  change displaces is kept in the runner's state folder (`fs-undo/`, 24 h, 50 MiB, oldest first) with a journal
  (`started` before, `done`/`failed` after, the path hashed). `fs.import`, `fs.root_add`, `fs.root_remove`: not built.

## Machine stats (Phase 20)

CPU, memory and disk of a machine, live in the terminal page's top bar (design: `docs/design/machine-hud.md`). The
machine is sampled **only while someone is looking**: nothing is read, timed or sent otherwise. Runner feature:
`stats` in `hello.runner.features`; a client hides the meter for a runner without it.

```
client  → gateway  {t:"stats", on: true|false, hz?: 0.1..0.5}     // subscribe (again: change the rate) / unsubscribe
gateway → runner   {t:"stats", hz}                                  // the fastest rate any subscriber asked; 0 = stop
runner  → gateway  {t:"stats", at, cpu?:{pct?, cores?, quota?, throttledPct?, load1?},
                    mem?:{used, limit, limited?, pressure10?, swapUsed?}, disks:[{id:"home", free, total}]}
gateway → client   the same, with machineId; the first one a socket gets also carries history:[[at, cpuPct|null, memPct|null], …]
```

- **Subscription.** `stats` is for a socket bound to the machine (a ticket with `mid`); a machine-list socket is
  answered `no_machine`. The check is the one glances pass (the machine's owner); a socket of anyone else is never
  counted and never sent a sample. A closed socket is an unsubscribe. The web client subscribes while the meter is on
  screen, its socket live and the tab visible (`document.visibilityState`), and unsubscribes when the tab is hidden or
  the meter unmounts.
- **Rate.** `hz` is clamped to 0.1..0.5 (default 0.5: one sample every 2 s). The gateway asks the runner for the
  fastest rate any subscriber wants and sends `hz: 0` when the last one leaves; a runner that reconnects is asked
  again only if someone still looks. A socket is sent a sample no more often than it asked, and none while its send
  buffer is full (a skipped sample is replaced by the next; nothing is queued).
- **Runner.** With no rate set the sampler does nothing: it has no timer of its own (it is polled from the tick the
  daemon already runs) and reads no file. A gateway disconnect clears the rate. The first sample after a start is
  sent at once and has no `cpu.pct` (a percentage needs two readings).
- **What the numbers are.** Inside a container they are the **container's**: memory is `memory.current` of
  `memory.max` of the nearest cgroup v2 directory with a numeric limit (the one the memory watchdog enforces),
  `limited: true`; CPU is the cgroup's `cpu.stat usage_usec` against its `cpu.max` quota (`quota`, in cores), with
  `throttledPct` from `nr_throttled / nr_periods`. On a machine without limits: `/proc/stat` deltas over all cores,
  `MemTotal − MemAvailable` of `MemTotal`, `load1` from `/proc/loadavg`. `pressure10` is PSI `some avg10` (the
  cgroup's `memory.pressure` when limited, `/proc/pressure/memory` otherwise). Disk is `statvfs` of the runner's
  home. Percentages have one decimal; bytes are integers.
- **Not available.** A field the machine cannot report is absent, never a guess: no `cpu.pct` on the first sample,
  no `cpu` or `mem` at all where there is no `/proc` (macOS today), an empty `disks` when the home cannot be read. A
  client draws a dash and says "not available".
- **Numbers only.** A sample holds numbers and the word `home`. No process name, command line, user name, host name,
  path or mount list is read or sent. The gateway does not forward the runner's text: it checks every field
  (`parseStatsSample`: ranges, safe integers, `free ≤ total`, at most 4 disks, ids `[a-z]{1,16}`) and rebuilds the
  sample, so anything else a runner adds is dropped; a frame over 4 KiB or with a bad field is counted
  (`bad_stats`, `stats_too_large`) and sent to nobody. The runner reads at most 64 KiB of any file, never follows a
  cgroup path with `..`, runs no program, and treats every file as possibly missing, short or junk.
- **Kept.** While anyone is subscribed the gateway holds the newest sample and up to 300 history points (10
  minutes; three numbers each) in memory, so a second viewer's graphs start filled. Both are forgotten when the
  last subscriber leaves and when the runner disconnects. Nothing is written anywhere; web stores nothing.
- **Thresholds** (shared by every client, `packages/protocol/src/stats.ts`): memory amber at 80 % of the limit or
  when `pressure10` is above 10, red at 90 % (the watchdog's threshold: it stops the largest process there); disk
  amber below 15 % free, red below 5 %; CPU amber only after 30 s at or above 85 %.
- **Metrics.** `tmuxedo_gateway_stats_viewers` (0 = no runner is sampling) and
  `tmuxedo_gateway_stats_frames_total{direction}`.

Fixtures: `packages/protocol/fixtures/stats.json` (both codecs).

## AI usage

How much of a plan's limits an agent CLI's account has used, as the CLI itself reported it (design:
`docs/design/agent-usage.md`). A runner that sends it lists the hello feature `usage`.

Runner → gateway, on change, at most one per account every 2 s, and every record again after a reconnect:

```
{ t: "usage", plugin, accountKey, mode: "plan" | "metered", plan?,
  windows: [{ id, usedPct, resetsAt?, windowMins? }],     // at most 8
  context?: { paneId, usedPct, size? }, reached?, source: "statusline" | "jsonl", observedAt }
```

- `accountKey` is opaque: `m:<32 hex>` is the machine's own key for that plugin (a hash of a random salt kept
  on the machine); `h:<hex>` is reserved for a salted hash of the vendor account. No account name, id or email.
- `windows[].id` is `5h`, `7d` or `<minutes>m`; `usedPct` is 0..100; `resetsAt` is Unix seconds; `observedAt` is
  Unix milliseconds. `reached` is the id of a window that is used up. `plan` is the plan's name (`plus`), display only.
- `context` is the context window of the pane that reported last.
- A value the CLI did not report is left out. Nothing else is in the frame: no tokens, keys, prompts, paths or
  vendor JSON. Both codecs check and rebuild a record (`UsageRecord::ok`, `parseUsage`); shared fixtures:
  `packages/protocol/fixtures/usage.json`. A frame over 4096 bytes is refused.

**The switch.** `config.plugins[].settings.usage.enabled` is the person's choice (Settings → Terminal, one switch for
every agent that can report). A runner reads usage for a plugin only while it is `true`; absent (an older web, or
no `config` yet) is **off**. Off means: the generated Claude settings have no `statusLine` key at all (they are
exactly the hooks, as before AI usage existed), no session log is read, what was read is forgotten, a report from
an agent that is still running with the old settings is dropped, and no `usage` frame is sent. The wish is kept in
`<state>/usage.json`, so a restarted runner writes the same settings before it hears from web again.

Gateway → client: `{ t: "usage", machineId, …the record }` (`UsageFrame`, `parseUsageFrame` in
`packages/protocol/src/usage.ts`), to the sockets of the machine's owner only (the check glances and stats pass),
on every change and, the newest record per account, to a socket that joins (after `welcome`). Clients ask for
nothing. The gateway:

- takes `usage` only from a runner whose hello lists the feature, at most 16 frames a second, at most 4096 bytes;
- checks every field and rebuilds the record (`parseUsage`): what it sends is that object, never the runner's text,
  so anything a runner wrote beside the documented fields is gone;
- passes it only for a plugin the owner's `config` turns usage on for (`usage_off` otherwise), and forgets what it
  holds for a plugin the moment a `config` turns it off;
- keeps the newest record per (machine, plugin, `accountKey`), at most 16 accounts a machine (the account heard
  from longest ago goes first), drops a record older than the one it holds, and forgets all of them when the
  runner's socket goes (a returning runner sends its records again);
- skips a socket whose send buffer is over the glance limit (the next change follows);
- logs nothing of a record; frames are counted (`tmuxedo_gateway_usage_frames_total`, refusals by reason).
