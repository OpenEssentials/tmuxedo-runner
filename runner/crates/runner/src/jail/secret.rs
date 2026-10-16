//! The secret-file deny list and the name folding it matches on
//! (docs/design/file-browser.md §3.4 step 2 and §3.5).
//!
//! A name is FOLDED before it is compared, so a spelling the filesystem or
//! a person would read as the same name cannot slip past the list:
//!
//! 1. Unicode NFKC (compatibility forms: a fullwidth `．ｅｎｖ` is `.env`);
//! 2. case folding, as upper-case then lower-case of each code point (no
//!    context rules, so both languages agree: `.ENV`, `ſ`, `K`);
//! 3. default-ignorable code points removed (zero-width space and joiners,
//!    soft hyphen, bidi marks, variation selectors, the BOM);
//! 4. trailing dots and spaces removed (Windows drops them).
//!
//! The pass runs twice (removing a code point can leave something NFKC
//! composes). `foldName` in `web/src/lib/chat-files.ts` is the same
//! function; both are held to `packages/protocol/fixtures/secret-files.json`.

use icu_normalizer::ComposingNormalizerBorrowed;

/// Unicode `Default_Ignorable_Code_Point` (DerivedCoreProperties.txt), as
/// inclusive ranges. The same table is in `web/src/lib/chat-files.ts`.
const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

fn default_ignorable(c: char) -> bool {
    let c = c as u32;
    DEFAULT_IGNORABLE.iter().any(|(a, b)| (*a..=*b).contains(&c))
}

fn fold_pass(nfkc: &ComposingNormalizerBorrowed<'static>, s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in nfkc.normalize(s).chars() {
        for u in c.to_uppercase() {
            out.extend(u.to_lowercase().filter(|l| !default_ignorable(*l)));
        }
    }
    out.truncate(out.trim_end_matches(['.', ' ']).len());
    out
}

/// One path component, folded for comparison (see the module text).
pub fn fold(component: &str) -> String {
    // Plain lower-case ASCII with nothing to trim is already folded (most names).
    if component.bytes().all(|b| b.is_ascii() && !b.is_ascii_uppercase()) && !component.ends_with(['.', ' ']) {
        return component.to_string();
    }
    let nfkc = ComposingNormalizerBorrowed::new_nfkc();
    let once = fold_pass(&nfkc, component);
    fold_pass(&nfkc, &once)
}

/// Folders whose whole content is credentials or internals, with the reason.
/// The folder's own name is refused too, so it is counted, not listed.
const SECRET_DIRS: &[(&str, &str)] = &[
    (".git", "git internals"),
    (".ssh", "credentials"),
    (".gnupg", "credentials"),
    (".aws", "credentials"),
    (".azure", "credentials"),
    (".kube", "credentials"),
    (".docker", "credentials"),
    (".gcloud", "credentials"),
    (".terraform", "infrastructure state"),
];

/// The same, for folders named by two components in a row.
const SECRET_DIR_PAIRS: &[(&str, &str, &str)] =
    &[(".config", "gcloud", "credentials"), (".config", "gh", "credentials")];

/// Why a path (relative, `/`-separated) may hold secrets and is never
/// shown, or None. Every component is folded first. A secret folder
/// refuses itself and everything beneath it.
pub fn secret_reason(rel: &str) -> Option<&'static str> {
    let parts: Vec<String> = rel.split('/').map(fold).filter(|p| !p.is_empty()).collect();
    let name = parts.last().map(String::as_str).unwrap_or("");
    for part in &parts {
        if let Some((_, why)) = SECRET_DIRS.iter().find(|(d, _)| d == part) {
            return Some(why);
        }
    }
    for pair in parts.windows(2) {
        if let Some((_, _, why)) = SECRET_DIR_PAIRS.iter().find(|(a, b, _)| *a == pair[0] && *b == pair[1]) {
            return Some(why);
        }
    }
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") || name == ".envrc" {
        return Some("environment file");
    }
    let key_ext = [
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".ppk",
        ".asc",
        ".gpg",
    ];
    if key_ext.iter().any(|e| name.ends_with(e)) {
        return Some("key file");
    }
    if ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|k| name.starts_with(k))
    {
        return Some("SSH key");
    }
    let creds = [
        ".netrc",
        "_netrc",
        ".npmrc",
        ".pypirc",
        ".pgpass",
        ".git-credentials",
        ".credentials.json",
        "credentials.json",
        "credentials",
        "auth.json",
        ".htpasswd",
        "secrets.yaml",
        "secrets.yml",
        "secrets.json",
        ".vault-token",
        "kubeconfig",
    ];
    if creds.contains(&name) || (name.starts_with("service-account") && name.ends_with(".json")) {
        return Some("credentials");
    }
    // `*.tfstate*` (the state, its backups and lock copies) and `*.tfvars`.
    if name.contains(".tfstate") || name.ends_with(".tfvars") {
        return Some("infrastructure state");
    }
    if name.ends_with(".kdbx") {
        return Some("password database");
    }
    if name.ends_with(".ovpn") {
        return Some("VPN profile");
    }
    // `.*_history`: what a shell or a database client remembers was typed.
    if name.starts_with('.') && name.ends_with("_history") {
        return Some("command history");
    }
    None
}
