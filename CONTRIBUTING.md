# Contributing

Thanks for helping. This repository holds the tmuxedo runner, holder, CLI, the wire protocol and the
tmuxedo for Mac app; the tmuxedo service builds its released binaries from the same code.

**How changes land.** This repository is published from tmuxedo's main development repository: every
commit on `main` is a `sync:` commit that carries the source commit in a `Tmuxedo-Source:` trailer.
Pull requests are reviewed and discussed here as usual; once one is accepted, a maintainer applies it
upstream and it arrives in the next sync (with you as the commit's author), and the pull request is
closed with a link to that commit.

## Before you start

- For anything larger than a small fix, open an issue first so we can agree on the approach.
- Security problems: do not open an issue; see [SECURITY.md](SECURITY.md).

## Making a change

1. Fork, branch from `main`, and keep each pull request to one change.
2. Run the checks CI runs (see the README):
   - `runner/`: `cargo fmt --all --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`,
     `cargo test --locked --workspace`, and the agent-detection coverage gate
     (`docker build -f runner/Dockerfile.downloads --target coverage .`: every line and every branch of
     `runner/crates/detect` covered);
   - `packages/protocol/`: `npm ci && npm run typecheck && npm test`;
   - `apps/mac/` (on a Mac): `fastlane mac test` and `fastlane mac build` (see
     [apps/mac/README.md](apps/mac/README.md));
   - `python3 scripts/scrub.py`: the publication gate (no internal hosts, private addresses or personal
     details).
3. **Wire format changes** touch the Rust code, the TypeScript code and the golden fixtures in
   `packages/protocol/fixtures/` together. Fixtures are generated (`npm run fixtures`), never edited by
   hand. Keep old runners and old clients working: new frames and fields are ignored by peers that do not
   know them, and a runner advertises what it serves in `hello.runner.features`.
4. Add a test that fails without your change.
5. **Agent detection** (`runner/crates/detect/`) follows `docs/design/agent-detection.md`. A change to an agent
   file comes with a stand-in screen under `runner/crates/detect/tests/screens/<agent>/` for every signal it adds
   or changes; the tests require every signal to decide at least one screen.
6. **Runner API** changes keep `runner/api/runner-api.schema.json` true: the conformance tests validate every
   request, response and event against it.

## Licensing of contributions

By submitting a contribution you agree that it is licensed under the Apache License, Version 2.0, as
section 5 of the license says, and that you have the right to submit it.

## Code of conduct

Be kind and assume good faith. Maintainers may remove comments or contributions that are abusive.
