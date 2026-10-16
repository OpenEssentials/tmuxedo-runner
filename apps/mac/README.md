# tmuxedo for Mac

A native macOS menu-bar app that keeps this Mac's terminals reachable from tmuxedo: from your
phone and any browser. It bundles the Rust `tmuxedo-runner` from this repository (it never
reimplements it) and supervises it; your terminals live in the runner's **holder**, which runs as
its own LaunchAgent and survives quitting the app and every update.

## What it does

- **Pair this Mac**, every way in one sheet: **Sign in with your iPhone** (scan the QR code with
  the tmuxedo app, approve there and type the 6-digit code shown here), a passkey, or the link in
  a browser. The credential is bound to a key in this Mac's Secure Enclave: a copied token is
  useless anywhere else.
- **Supervises the runner**: starts it `--managed` (no self-update; the app updates as a whole),
  restarts it with backoff, stops for good when the server refuses this Mac, shows its log.
- **The holder**: registered with `SMAppService` from `Contents/Library/LaunchAgents/`;
  restarted only when the holder's own code changed (its build fingerprint), so an ordinary
  update keeps every terminal.
- **Dashboard**: machine status, workspaces and agents (working / needs you / done / idle) from
  the runner's local API, links into tmuxedo, the log, About (the licences of what it bundles).
- **Settings**: work folder, start at login, connect at launch, keep awake, notifications,
  updates, Unpair; a **kill switch** that stops runner and holder and deletes the credential
  here, without needing the server.
- **Isolation** (Settings → Isolation): new installs run terminals as a hidden, standard macOS
  user `tmuxedo` that cannot read your home, `~/.ssh` or Keychain or use sudo, and reaches only
  the project folders you grant (one administrator approval for the privileged helper
  `tmuxedo-helper`). Full access (as you) is opt-in behind a warning. Optional **confirm on this
  Mac**: Allow / Deny before remote control. `Tools/isolation-e2e.sh <app>` proves the boundary
  on a Mac with passwordless sudo (CI does).
- **Moving from `install.sh`**: retires the old runner service, leaves the old holder (and its
  sessions) alone until it exits, and can adopt the existing enrolment.

It needs macOS 14 or newer. It can never be an App Store app: the terminals it serves run your
own shells and agents, outside what the App Sandbox permits.

## Build and test

You need Xcode 16 or newer, [XcodeGen](https://github.com/yonaskolb/XcodeGen) and Rust (see the
repository README). From this folder:

```sh
brew install xcodegen fastlane
fastlane mac test        # XCTest with coverage (build/coverage.json)
fastlane mac build       # unsigned compile check
```

Or with plain Xcode tools:

```sh
xcodegen generate
xcodebuild build -project Tmuxedo.xcodeproj -scheme Tmuxedo -configuration Release \
  -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO
xcodebuild test -project Tmuxedo.xcodeproj -scheme Tmuxedo -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO
```

The runner is embedded by `Scripts/embed-runner.sh`: `TMUXEDO_RUNNER_BIN` (a prebuilt binary),
else `cargo` builds this repository's `runner/` at the same commit (both architectures, lipo), else
the published binaries from `tmuxedo.dev/dl` (sha256-checked against its manifest).

`TMUXEDO_TEST_RUNNER=<binary>` turns on the live tests (`Tests/LiveRunnerTests.swift`): the real
runner, paired and connected through `Tools/mock-host.py`, its key proof verified.
`Tools/holder-e2e.sh <binary>` checks the holder under launchd: it survives an app update and is
restarted only on purpose. `Tools/isolation-e2e.sh <app>` creates the separate user with the
app's own helper and tries every boundary (it needs passwordless sudo; run it on a throwaway
Mac or VM, as CI does).

CI (`.github/workflows/ci.yml`, job `mac-app`) does all of this on GitHub-hosted macOS runners:
the universal runner from the same commit, the unsigned app with the runner embedded, the
XCTest suite, the launchd checks and the isolation abuse cases.

## Signing your own build

Builds from this repository are **unsigned** (or ad-hoc signed) and have no update feed, so
they never update themselves. An unsigned app runs only an unsigned runner, and its helper
serves nobody (it cannot check who calls); separate-user mode needs a signed build.

To sign with your own Apple Developer ID:

1. Create `Signing.local.xcconfig` in this folder (gitignored), read by `Signing.xcconfig`:

   ```
   DEVELOPMENT_TEAM = ABCDE12345
   ```

2. `DEVELOPMENT_TEAM=ABCDE12345 fastlane mac signed` builds with your "Developer ID Application"
   identity and checks the result (hardened runtime, secure timestamp, no `get-task-allow`, the
   runner signed by the same team, the LaunchAgent).
3. The push and associated-domain entitlements need a Developer ID provisioning profile for your
   own bundle identifier; change `PRODUCT_BUNDLE_IDENTIFIER` in `project.yml` if you are not
   building tmuxedo's own release. Notarising (`xcrun notarytool`) and stapling are then yours.

Updates: the app updates through Sparkle **only** from the feed its build carries in its
Info.plist (`SUFeedURL`, with the EdDSA key `SUPublicEDKey`). Neither is committed, so a build
from source has no feed. A release pipeline that writes both into the plist makes its own builds
update from its own appcast, with signatures checked against its own key.

## Releases

tmuxedo's own signed and notarized builds (Developer ID, hardened runtime, stapled) are published
on this repository's Releases page as `mac-v<version>-<build>`: `tmuxedo-mac-<version>-<build>.pkg`
(installs into /Applications), the same app as `.zip`, and `SHA256SUMS`. Check a download with:

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing
pkgutil --check-signature tmuxedo-mac-*.pkg
spctl -a -t install -vv tmuxedo-mac-*.pkg
```

## License

Apache License 2.0, like the rest of this repository (see `LICENSE` and `NOTICE` at the root). The
app bundles [Sparkle](https://sparkle-project.org) (MIT licence), fetched by Swift Package Manager at
build time.
