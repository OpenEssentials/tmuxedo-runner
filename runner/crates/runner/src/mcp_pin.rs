//! Which package an MCP server's command fetches, and whether it is pinned
//! to one exact version (PLAN Phase 17 box 5; docs/protocol.md "Pinned
//! packages"). The same reading as `mcpPackage` in
//! `packages/protocol/src/mcp-pin.ts`, checked on the shared
//! `packages/protocol/fixtures/mcp-pin.json`.
//!
//! Pinned: npm `pkg@1.2.3` (an exact semantic version), PyPI `pkg==1.2.3` or
//! `pkg@1.2.3`, an image `name@sha256:<64 hex>`. A range, a tag (`latest`,
//! an image tag) or no version is not pinned: what runs could change under
//! the person between two launches. The builders pass the argv through
//! element by element (no shell), so a pinned launch runs exactly what was
//! pinned; an unpinned one is still built (web refuses new ones; older items
//! are flagged there) and `report.json` says so.

use std::sync::LazyLock;

use regex::Regex;

/// A package launch: the registry, the package or image, its exact version
/// or digest when pinned, and otherwise why it is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagePin {
    pub registry: &'static str,
    pub name: String,
    pub version: Option<String>,
    /// `missing`, `range`, `tag`, `not_registry` or `bad_name`; None when pinned.
    pub problem: Option<&'static str>,
}

impl PackagePin {
    pub fn pinned(&self) -> bool {
        self.problem.is_none()
    }
}

fn re(s: &str) -> Regex {
    Regex::new(s).expect("a fixed pattern")
}

static NPM_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:@[a-z0-9][a-z0-9._~-]*/)?[a-z0-9][a-z0-9._~-]*$"));
static PYPI_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,98}[A-Za-z0-9])?$"));
static OCI_NAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[a-z0-9][a-z0-9._-]*(?::[0-9]{1,5})?(?:/[a-z0-9][a-z0-9._-]*)*$"));
static OCI_TAG: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$"));
static NPM_EXACT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$",
    )
});
static PYPI_EXACT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^(?:[0-9]+!)?[0-9]+(?:\.[0-9]+)*(?:(?:a|b|rc)[0-9]+)?(?:\.post[0-9]+)?(?:\.dev[0-9]+)?(?:\+[a-z0-9]+(?:\.[a-z0-9]+)*)?$",
    )
});
static OCI_DIGEST: LazyLock<Regex> = LazyLock::new(|| re(r"^sha256:[a-f0-9]{64}$"));
static TAG_WORD: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z][A-Za-z0-9._-]*$"));
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^[a-z][a-z0-9+.-]*:"));
static SHORTHAND: LazyLock<Regex> = LazyLock::new(|| re(r"^[^@/][^/]*/."));
static PYPI_SPEC: LazyLock<Regex> = LazyLock::new(|| re(r"(?s)^([^\[=<>!~@;\s]*)(\[[A-Za-z0-9._,\s-]*\])?\s*(.*)$"));

const NPX_VALUED: &[&str] = &[
    "-p",
    "--package",
    "-c",
    "--call",
    "--registry",
    "--cache",
    "--userconfig",
    "-w",
    "--workspace",
];
const UVX_VALUED: &[&str] = &[
    "--from",
    "--with",
    "-w",
    "--with-requirements",
    "--with-editable",
    "--python",
    "-p",
    "--index",
    "--index-url",
    "-i",
    "--extra-index-url",
    "--default-index",
    "--find-links",
    "-f",
    "--constraints",
    "-c",
    "--overrides",
    "--cache-dir",
    "--directory",
    "--project",
    "--config-file",
    "--env-file",
    "--prerelease",
    "--resolution",
    "--exclude-newer",
    "--refresh-package",
    "--upgrade-package",
    "-P",
    "--reinstall-package",
    "--color",
    "--python-preference",
    "--index-strategy",
    "--keyring-provider",
    "--link-mode",
];
const PIPX_VALUED: &[&str] = &["--spec", "--python", "--pip-args", "--index-url", "-i", "--backend"];
const DOCKER_VALUED: &[&str] = &[
    "-a",
    "--attach",
    "--add-host",
    "--cap-add",
    "--cap-drop",
    "--cidfile",
    "--cpus",
    "--cpu-shares",
    "-c",
    "--cpuset-cpus",
    "--device",
    "--dns",
    "-e",
    "--env",
    "--env-file",
    "--entrypoint",
    "--expose",
    "--group-add",
    "-h",
    "--hostname",
    "--ip",
    "-l",
    "--label",
    "--label-file",
    "--log-driver",
    "--log-opt",
    "-m",
    "--memory",
    "--mount",
    "--name",
    "--network",
    "--net",
    "--network-alias",
    "-p",
    "--publish",
    "--platform",
    "--pull",
    "--restart",
    "--runtime",
    "--security-opt",
    "--shm-size",
    "--stop-signal",
    "--stop-timeout",
    "--tmpfs",
    "--ulimit",
    "-u",
    "--user",
    "--userns",
    "-v",
    "--volume",
    "--volumes-from",
    "-w",
    "--workdir",
    "--health-cmd",
    "--ipc",
    "--pid",
    "--uts",
    "--gpus",
    "--isolation",
    "--sysctl",
];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Npx,
    Uvx,
    Pipx,
    Docker,
}

fn launcher(argv: &[String]) -> Option<(Kind, usize)> {
    let first = argv.first()?;
    let base = first.rsplit('/').next().unwrap_or(first).to_ascii_lowercase();
    let base = base
        .strip_suffix(".cmd")
        .or_else(|| base.strip_suffix(".exe"))
        .unwrap_or(&base);
    let sub = argv.get(1).map(String::as_str);
    let third = argv.get(2).map(String::as_str);
    match (base, sub, third) {
        ("npx" | "bunx", _, _) => Some((Kind::Npx, 1)),
        ("pnpm" | "yarn", Some("dlx"), _) | ("npm", Some("exec" | "x"), _) => Some((Kind::Npx, 2)),
        ("uvx", _, _) => Some((Kind::Uvx, 1)),
        ("uv", Some("tool"), Some("run")) => Some((Kind::Uvx, 3)),
        ("pipx", Some("run"), _) => Some((Kind::Pipx, 2)),
        ("docker" | "podman", Some("run"), _) => Some((Kind::Docker, 2)),
        ("docker" | "podman", Some("container"), Some("run")) => Some((Kind::Docker, 3)),
        _ => None,
    }
}

fn is_local(s: &str) -> bool {
    s.starts_with("./") || s.starts_with("../") || s == "." || s == ".." || s.starts_with('/') || s.starts_with('~')
}

fn pin(registry: &'static str, name: &str, version: Option<&str>, problem: Option<&'static str>) -> PackagePin {
    PackagePin {
        registry,
        name: name.to_string(),
        version: version.map(str::to_string),
        problem,
    }
}

fn npm(s: &str) -> Option<PackagePin> {
    if is_local(s) {
        return None;
    }
    if ADDRESS.is_match(s) || SHORTHAND.is_match(s) {
        return Some(pin("npm", s, None, Some("not_registry")));
    }
    let at = s.char_indices().skip(1).find(|(_, c)| *c == '@').map(|(i, _)| i);
    let (name, spec) = match at {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    if name.is_empty() || name.len() > 214 || !NPM_NAME.is_match(name) {
        return Some(pin("npm", name, None, Some("bad_name")));
    }
    Some(match spec {
        None => pin("npm", name, None, Some("missing")),
        Some(v) if NPM_EXACT.is_match(v) => pin("npm", name, Some(v), None),
        Some(v) if TAG_WORD.is_match(v) => pin("npm", name, None, Some("tag")),
        Some(_) => pin("npm", name, None, Some("range")),
    })
}

fn pypi(s: &str) -> Option<PackagePin> {
    if is_local(s) {
        return None;
    }
    if ADDRESS.is_match(s) || s.contains(" @ ") {
        return Some(pin("pypi", s, None, Some("not_registry")));
    }
    let caps = PYPI_SPEC.captures(s)?;
    let name = caps.get(1).map_or("", |m| m.as_str());
    let rest = caps.get(3).map_or("", |m| m.as_str()).trim();
    let (sep, spec) = if rest.starts_with("==") && !rest.starts_with("===") {
        ("==", rest[2..].trim())
    } else if let Some(v) = rest.strip_prefix('@') {
        ("@", v.trim())
    } else {
        ("", rest)
    };
    if !PYPI_NAME.is_match(name) {
        return Some(pin("pypi", name, None, Some("bad_name")));
    }
    Some(if rest.is_empty() {
        pin("pypi", name, None, Some("missing"))
    } else if !sep.is_empty() && PYPI_EXACT.is_match(spec) {
        pin("pypi", name, Some(spec), None)
    } else if sep == "@" && TAG_WORD.is_match(spec) {
        pin("pypi", name, None, Some("tag"))
    } else {
        pin("pypi", name, None, Some("range"))
    })
}

fn oci(s: &str) -> PackagePin {
    let (reference, digest) = match s.find('@') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let slash = reference.rfind('/');
    let colon = reference.rfind(':').filter(|c| slash.is_none_or(|sl| *c > sl));
    let (name, tag) = match colon {
        Some(c) => (&reference[..c], Some(&reference[c + 1..])),
        None => (reference, None),
    };
    if name.is_empty() || name.len() > 255 || !OCI_NAME.is_match(name) || tag.is_some_and(|t| !OCI_TAG.is_match(t)) {
        return pin("oci", name, None, Some("bad_name"));
    }
    match (digest, tag) {
        (Some(d), _) if OCI_DIGEST.is_match(d) => pin("oci", name, Some(d), None),
        (Some(_), _) => pin("oci", name, None, Some("bad_name")),
        (None, Some(_)) => pin("oci", name, None, Some("tag")),
        (None, None) => pin("oci", name, None, Some("missing")),
    }
}

/// The package a stdio server's command fetches, or None when the command is
/// not a package launch (another program, or a local path).
pub fn package_pin(argv: &[String]) -> Option<PackagePin> {
    let (kind, start) = launcher(argv)?;
    let (valued, package_opt): (&[&str], &[&str]) = match kind {
        Kind::Npx => (NPX_VALUED, &["-p", "--package"]),
        Kind::Uvx => (UVX_VALUED, &["--from"]),
        Kind::Pipx => (PIPX_VALUED, &["--spec"]),
        Kind::Docker => (DOCKER_VALUED, &[]),
    };
    let mut found: Option<&str> = None;
    let mut i = start;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--" {
            if found.is_none() {
                found = argv.get(i + 1).map(String::as_str);
            }
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            let (opt, inline) = match a.find('=') {
                Some(e) => (&a[..e], Some(&a[e + 1..])),
                None => (a, None),
            };
            if found.is_none() && package_opt.contains(&opt) {
                found = inline.or_else(|| argv.get(i + 1).map(String::as_str));
            }
            if inline.is_none() && valued.contains(&opt) {
                i += 1;
            }
            i += 1;
            continue;
        }
        if found.is_none() {
            found = Some(a);
        }
        break;
    }
    let spec = found?;
    match kind {
        Kind::Npx => npm(spec),
        Kind::Uvx | Kind::Pipx => pypi(spec),
        Kind::Docker => Some(oci(spec)),
    }
}
