# The runner API

`runner-api.schema.json` (JSON Schema 2020-12) specifies tmuxedo's runner API: the
newline-delimited JSON the runner serves on its local socket (`$TMUXEDO_SOCKET_PATH`)
and the same methods relayed in gateway `req` / `res` frames (docs/protocol.md
"Methods").

| Schema | What it validates |
|---|---|
| `#/schemas/request` | `{"id", "method", "params"}`; `methods` lists every method, each with its parameters |
| `#/schemas/success_response` | `{"id", "result"}`; one variant per `result.type` |
| `#/schemas/error_response` | `{"id", "error": {"code", "message"}}` |
| `#/schemas/event` | lifecycle events on an `events.subscribe` stream: `{"event": "pane_created", "data": {"type": …}}` |
| `#/schemas/subscription_event` | `pane.agent_status_changed` and `pane.output_matched` |

Shared shapes (pane, tab, workspace, agent, layout nodes, …) are under `$defs`.
Parameters and results may carry more fields than the schema names; clients must
ignore fields they do not know.

## How it is kept true

- `crates/runner/tests/conformance.rs` has one test per method. Every request it
  sends and every answer and event it reads is validated against this schema, and a
  test checks that the schema lists exactly the methods the runner implements.
- To review the schema against everything the test suite exchanges, run the suite
  with `TMUXEDO_API_RECORD=<file>`: every request/response pair and every event the
  tests read is appended to `<file>` as one JSON object per line.
