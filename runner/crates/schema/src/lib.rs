//! The restricted JSON Schema dialect of a plugin's `settings.schema.json`,
//! and its validator (docs/protocol.md "Plugin settings").
//!
//! The dialect is a small subset of JSON Schema 2020-12 that every client can
//! draw as a form: strings, booleans, numbers, lists of strings and one level
//! of nested objects. It is an allow-list: a keyword that is not named here
//! is refused, so a schema never means more than a client shows.
//!
//! One implementation serves everyone: the runner links this crate, and web
//! loads the same code as WebAssembly (crate `tmuxedo-schema-wasm`). The
//! cases both must agree on are in
//! `packages/protocol/fixtures/settings-schema.json`.
//!
//! Limits are hard and checked before anything else, so a hostile schema or
//! value costs a bounded amount of work: sizes, nesting, property counts,
//! and the size of every compiled pattern (patterns run in linear time).

use regex::Regex;
use regex::RegexBuilder;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;

/// A schema, serialised, may be at most this long.
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// A value, serialised, may be at most this long.
pub const MAX_VALUE_BYTES: usize = 256 * 1024;
/// Containers nested in a schema document (objects and arrays).
pub const MAX_SCHEMA_DEPTH: usize = 8;
/// Containers nested in a value: the root, a section, a list.
pub const MAX_VALUE_DEPTH: usize = 3;
/// Properties of one object.
pub const MAX_PROPERTIES: usize = 64;
/// Schema nodes in one document.
pub const MAX_NODES: usize = 256;
/// Choices of one `enum`.
pub const MAX_ENUM: usize = 64;
/// Length of one `enum` choice.
pub const MAX_ENUM_CHARS: usize = 256;
/// Length of one `pattern`.
pub const MAX_PATTERN_BYTES: usize = 256;
/// Patterns in one document.
pub const MAX_PATTERNS: usize = 32;
/// Compiled size of one pattern.
pub const MAX_PATTERN_PROGRAM_BYTES: usize = 64 * 1024;
/// The largest `maxLength`, and the length of multi-line text without one.
pub const MAX_STRING_CHARS: usize = 64 * 1024;
/// The length of a one-line string without `maxLength`.
pub const DEFAULT_LINE_CHARS: usize = 4096;
/// The length of a `format: uri` string without `maxLength`.
pub const DEFAULT_URI_CHARS: usize = 2048;
/// Items of one list.
pub const MAX_ITEMS: usize = 256;
/// Length of `title`, `description` and `x-dangerous`.
pub const MAX_TEXT_CHARS: usize = 512;

/// The only `$schema` a document may name.
pub const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

const COMMON_KEYS: [&str; 8] = [
    "type",
    "title",
    "description",
    "default",
    "x-scope",
    "x-dangerous",
    "x-requires-plugin-version",
    "x-requires-agent-version",
];
const STRING_KEYS: [&str; 5] = ["enum", "pattern", "maxLength", "format", "x-secret-ref"];
const NUMBER_KEYS: [&str; 2] = ["minimum", "maximum"];
const ARRAY_KEYS: [&str; 2] = ["items", "x-argv"];
const OBJECT_KEYS: [&str; 3] = ["properties", "required", "additionalProperties"];
const NO_KEYS: [&str; 0] = [];
const ITEM_KEYS: [&str; 4] = ["type", "enum", "pattern", "maxLength"];
const SCOPES: [&str; 3] = ["user", "machine", "project"];

/// One thing wrong with a schema or a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Where: a JSON Pointer into the schema or the value (`""` = the root).
    pub path: String,
    /// What, as a stable word clients can switch on.
    pub code: &'static str,
    /// What, for a person.
    pub message: String,
}

/// What [`validate`] found: `value` is checked only against a clean schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub schema: Vec<Problem>,
    pub value: Vec<Problem>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.schema.is_empty() && self.value.is_empty()
    }

    /// `{"schema": [{path, code, message}], "value": [...]}`.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("schema".to_string(), problems_json(&self.schema));
        out.insert("value".to_string(), problems_json(&self.value));
        Value::Object(out)
    }
}

fn problems_json(list: &[Problem]) -> Value {
    let mut out = Vec::new();
    for p in list {
        out.push(json!({ "path": p.path, "code": p.code, "message": p.message }));
    }
    Value::Array(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Line,
    Uri,
    Multiline,
    Markdown,
}

#[derive(Debug, Clone)]
struct StrRule {
    choices: Vec<String>,
    pattern: Option<Regex>,
    max_chars: usize,
    format: Format,
    secret_ref: bool,
}

#[derive(Debug, Clone)]
struct ObjRule {
    props: Vec<(String, Node)>,
    required: Vec<String>,
}

#[derive(Debug, Clone)]
enum Node {
    Str(StrRule),
    Bool,
    Num {
        integer: bool,
        min: Option<f64>,
        max: Option<f64>,
    },
    List(StrRule),
    Obj(ObjRule),
}

/// A schema that passed the dialect check.
#[derive(Debug, Clone)]
pub struct Schema {
    root: ObjRule,
}

struct Ctx {
    problems: Vec<Problem>,
    nodes: usize,
    patterns: usize,
}

impl Ctx {
    fn add(&mut self, path: &str, code: &'static str, message: &str) {
        push(&mut self.problems, path, code, message);
    }
}

fn push(out: &mut Vec<Problem>, path: &str, code: &'static str, message: &str) {
    out.push(Problem {
        path: path.to_string(),
        code,
        message: message.to_string(),
    });
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(o) => 1 + o.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn bytes(v: &Value) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn is_property_name(s: &str) -> bool {
    let mut it = s.chars();
    let first = it.next().is_some_and(|c| c.is_ascii_alphabetic());
    first && s.len() <= 64 && it.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A secret is named like a shell variable (`GH_TOKEN`).
pub fn is_secret_name(s: &str) -> bool {
    let mut it = s.chars();
    let first = it.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first && s.len() <= 64 && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_version(s: &str) -> bool {
    let mut parts = 0;
    for p in s.split('.') {
        parts += 1;
        if p.is_empty() || p.len() > 6 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    parts <= 3
}

fn is_http_uri(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
    let clean = !s.chars().any(|c| c.is_whitespace() || c.is_control());
    clean && rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
}

fn is_integral(n: &serde_json::Number) -> bool {
    if n.is_i64() || n.is_u64() {
        return true;
    }
    let Some(f) = n.as_f64() else { return false };
    f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0
}

fn text_ok(v: &Value) -> bool {
    let s = v.as_str().unwrap_or("");
    !s.is_empty() && chars(s) <= MAX_TEXT_CHARS
}

/// The dialect check: a [`Schema`], or everything that is wrong with it.
pub fn compile(schema: &Value) -> Result<Schema, Vec<Problem>> {
    let mut cx = Ctx {
        problems: Vec::new(),
        nodes: 0,
        patterns: 0,
    };
    if bytes(schema) > MAX_SCHEMA_BYTES {
        cx.add("", "too_large", "the schema is larger than 64 KiB");
        return Err(cx.problems);
    }
    if depth(schema) > MAX_SCHEMA_DEPTH {
        cx.add("", "too_deep", "the schema nests too deeply");
        return Err(cx.problems);
    }
    let node = compile_node(schema, "", 0, &mut cx);
    if let Some(Node::Obj(root)) = node {
        if cx.problems.is_empty() {
            return Ok(Schema { root });
        }
    } else if node.is_some() {
        let msg = "the root of a settings schema must be an object";
        cx.add("", "root_type", msg);
    }
    Err(cx.problems)
}

fn compile_node(v: &Value, path: &str, level: usize, cx: &mut Ctx) -> Option<Node> {
    let Some(o) = v.as_object() else {
        cx.add(path, "schema_type", "a schema must be an object");
        return None;
    };
    cx.nodes += 1;
    if cx.nodes > MAX_NODES {
        if cx.nodes == MAX_NODES + 1 {
            cx.add(path, "too_many_nodes", "the schema has more than 256 nodes");
        }
        return None;
    }
    let kind = o.get("type").and_then(Value::as_str).unwrap_or("");
    let extra: &[&str] = match kind {
        "string" => &STRING_KEYS,
        "boolean" => &NO_KEYS,
        "integer" | "number" => &NUMBER_KEYS,
        "array" => &ARRAY_KEYS,
        "object" => &OBJECT_KEYS,
        _ => {
            let msg = "type must be one of string, boolean, integer, number, array, object";
            cx.add(path, "type", msg);
            return None;
        }
    };
    for key in o.keys() {
        let k = key.as_str();
        let root_only = level == 0 && k == "$schema";
        if !(root_only || COMMON_KEYS.contains(&k) || extra.contains(&k)) {
            let msg = format!("the keyword {k:?} is not allowed here");
            cx.add(path, "keyword_not_allowed", &msg);
        }
    }
    annotations(o, path, level, cx);
    let node = match kind {
        "string" => Some(Node::Str(str_rule(o, path, true, cx))),
        "boolean" => Some(Node::Bool),
        "array" => list_rule(o, path, cx).map(Node::List),
        "object" => obj_rule(o, path, level, cx).map(Node::Obj),
        _ => Some(num_rule(o, path, kind == "integer", cx)),
    };
    if let (Some(n), Some(d)) = (&node, o.get("default")) {
        let mut found = Vec::new();
        check_node(n, d, path, &mut found);
        if !found.is_empty() {
            cx.add(path, "default_invalid", "default does not fit this schema");
        }
    }
    node
}

fn annotations(o: &Map<String, Value>, path: &str, level: usize, cx: &mut Ctx) {
    let named = if level == 0 { o.get("$schema") } else { None };
    if named.is_some_and(|s| s.as_str() != Some(DIALECT)) {
        cx.add(path, "dialect", "$schema must be JSON Schema 2020-12");
    }
    for key in ["title", "description", "x-dangerous"] {
        if o.get(key).is_some_and(|t| !text_ok(t)) {
            let msg = format!("{key} must be text of 1 to 512 characters");
            cx.add(path, "annotation", &msg);
        }
    }
    for key in ["x-requires-plugin-version", "x-requires-agent-version"] {
        if o.get(key).is_some_and(|t| !t.as_str().is_some_and(is_version)) {
            let msg = format!("{key} must be a version like 1.2.3");
            cx.add(path, "annotation", &msg);
        }
    }
    if let Some(s) = o.get("x-scope") {
        let mut seen: Vec<&str> = Vec::new();
        let mut good = false;
        if let Some(list) = s.as_array() {
            good = !list.is_empty();
            for item in list {
                match item.as_str() {
                    Some(x) if SCOPES.contains(&x) && !seen.contains(&x) => seen.push(x),
                    _ => good = false,
                }
            }
        }
        if !good {
            let msg = "x-scope must list some of user, machine, project, each once";
            cx.add(path, "annotation", msg);
        }
    }
}

fn str_rule(o: &Map<String, Value>, path: &str, full: bool, cx: &mut Ctx) -> StrRule {
    let mut rule = StrRule {
        choices: Vec::new(),
        pattern: None,
        max_chars: DEFAULT_LINE_CHARS,
        format: Format::Line,
        secret_ref: false,
    };
    if full {
        match o.get("format").map(Value::as_str) {
            None => {}
            Some(Some("uri")) => rule.format = Format::Uri,
            Some(Some("multiline")) => rule.format = Format::Multiline,
            Some(Some("markdown")) => rule.format = Format::Markdown,
            Some(_) => cx.add(path, "format", "format must be uri, multiline or markdown"),
        }
        match o.get("x-secret-ref").map(Value::as_bool) {
            None => {}
            Some(Some(b)) => rule.secret_ref = b,
            Some(None) => cx.add(path, "annotation", "x-secret-ref must be true or false"),
        }
    }
    rule.max_chars = match rule.format {
        Format::Line => DEFAULT_LINE_CHARS,
        Format::Uri => DEFAULT_URI_CHARS,
        Format::Multiline | Format::Markdown => MAX_STRING_CHARS,
    };
    if let Some(m) = o.get("maxLength") {
        let msg = "maxLength must be a whole number from 1 to 65536";
        match m.as_u64() {
            Some(n) if (1..=MAX_STRING_CHARS as u64).contains(&n) => rule.max_chars = n as usize,
            _ => cx.add(path, "max_length", msg),
        }
    }
    if let Some(e) = o.get("enum") {
        let mut good = false;
        if let Some(list) = e.as_array() {
            good = !list.is_empty() && list.len() <= MAX_ENUM;
            for item in list {
                let s = item.as_str().unwrap_or("");
                let fresh = !rule.choices.iter().any(|c| c == s);
                if item.is_string() && fresh && chars(s) <= MAX_ENUM_CHARS {
                    rule.choices.push(s.to_string());
                } else {
                    good = false;
                }
            }
        }
        if !good {
            let msg = "enum must be 1 to 64 different strings of at most 256 characters";
            cx.add(path, "enum", msg);
        }
    }
    if let Some(p) = o.get("pattern") {
        rule.pattern = pattern(p, path, cx);
    }
    let others = ["enum", "pattern", "format", "default"];
    if rule.secret_ref && others.iter().any(|k| o.contains_key(*k)) {
        let msg = "x-secret-ref cannot be combined with enum, pattern, format or default";
        cx.add(path, "conflict", msg);
    }
    rule
}

fn pattern(p: &Value, path: &str, cx: &mut Ctx) -> Option<Regex> {
    let Some(src) = p.as_str() else {
        cx.add(path, "pattern_invalid", "pattern must be a string");
        return None;
    };
    cx.patterns += 1;
    if cx.patterns > MAX_PATTERNS {
        let msg = "the schema has more than 32 patterns";
        cx.add(path, "too_many_patterns", msg);
        return None;
    }
    if src.len() > MAX_PATTERN_BYTES {
        cx.add(path, "pattern_too_long", "pattern is longer than 256 bytes");
        return None;
    }
    let mut builder = RegexBuilder::new(src);
    builder.size_limit(MAX_PATTERN_PROGRAM_BYTES);
    builder.dfa_size_limit(MAX_PATTERN_PROGRAM_BYTES);
    builder.nest_limit(32);
    match builder.build() {
        Ok(re) => Some(re),
        Err(regex::Error::CompiledTooBig(_)) => {
            let msg = "pattern compiles to more than 64 KiB";
            cx.add(path, "pattern_too_costly", msg);
            None
        }
        Err(_) => {
            let msg = "pattern is not supported (no look-around, no back-references)";
            cx.add(path, "pattern_invalid", msg);
            None
        }
    }
}

fn num_rule(o: &Map<String, Value>, path: &str, integer: bool, cx: &mut Ctx) -> Node {
    let mut bounds: [Option<f64>; 2] = [None, None];
    for (i, key) in NUMBER_KEYS.iter().enumerate() {
        let Some(v) = o.get(*key) else { continue };
        if v.as_number().is_some_and(|n| !integer || is_integral(n)) {
            bounds[i] = v.as_f64();
        } else {
            let msg = format!("{key} must be a number of this type");
            cx.add(path, "bound", &msg);
        }
    }
    if let (Some(lo), Some(hi)) = (bounds[0], bounds[1]) {
        if lo > hi {
            cx.add(path, "bound", "minimum is above maximum");
        }
    }
    Node::Num {
        integer,
        min: bounds[0],
        max: bounds[1],
    }
}

fn list_rule(o: &Map<String, Value>, path: &str, cx: &mut Ctx) -> Option<StrRule> {
    if o.get("x-argv").is_some_and(|a| !a.is_boolean()) {
        cx.add(path, "annotation", "x-argv must be true or false");
    }
    let at = format!("{path}/items");
    let Some(items) = o.get("items").and_then(Value::as_object) else {
        cx.add(&at, "items", "an array needs items of type string");
        return None;
    };
    cx.nodes += 1;
    if items.get("type").and_then(Value::as_str) != Some("string") {
        cx.add(&at, "items", "only arrays of strings are allowed");
        return None;
    }
    for key in items.keys() {
        if !ITEM_KEYS.contains(&key.as_str()) {
            let msg = format!("the keyword {key:?} is not allowed here");
            cx.add(&at, "keyword_not_allowed", &msg);
        }
    }
    Some(str_rule(items, &at, false, cx))
}

fn obj_rule(o: &Map<String, Value>, path: &str, level: usize, cx: &mut Ctx) -> Option<ObjRule> {
    if level > 1 {
        cx.add(path, "nesting", "objects nest one level only");
        return None;
    }
    let open = o.get("additionalProperties");
    if open.is_some_and(|a| a.as_bool() != Some(false)) {
        let msg = "additionalProperties may only be false (it always is)";
        cx.add(path, "keyword_not_allowed", msg);
    }
    let Some(props) = o.get("properties").and_then(Value::as_object) else {
        cx.add(path, "properties", "an object needs properties");
        return None;
    };
    if props.len() > MAX_PROPERTIES {
        let msg = "an object has more than 64 properties";
        cx.add(path, "too_many_properties", msg);
        return None;
    }
    let mut rule = ObjRule {
        props: Vec::new(),
        required: Vec::new(),
    };
    for (name, sub) in props {
        let at = format!("{path}/properties/{name}");
        if !is_property_name(name) {
            let msg = "a property name is a letter, then letters, digits, - or _ (64 at most)";
            cx.add(&at, "property_name", msg);
            continue;
        }
        if let Some(node) = compile_node(sub, &at, level + 1, cx) {
            rule.props.push((name.clone(), node));
        }
    }
    if let Some(r) = o.get("required") {
        let mut good = false;
        if let Some(list) = r.as_array() {
            good = true;
            for item in list {
                let s = item.as_str().unwrap_or("");
                let fresh = !rule.required.iter().any(|x| x == s);
                if fresh && props.contains_key(s) {
                    rule.required.push(s.to_string());
                } else {
                    good = false;
                }
            }
        }
        if !good {
            let msg = "required must list declared properties, each once";
            cx.add(path, "required", msg);
        }
    }
    Some(rule)
}

impl Schema {
    /// Everything wrong with `value` under this schema (empty = valid).
    pub fn check(&self, value: &Value) -> Vec<Problem> {
        let mut out = check_value_limits(value);
        if out.is_empty() {
            check_obj(&self.root, value, "", &mut out);
        }
        out
    }
}

/// The limits every settings value must keep, with or without a schema.
pub fn check_value_limits(value: &Value) -> Vec<Problem> {
    let mut out = Vec::new();
    if bytes(value) > MAX_VALUE_BYTES {
        push(&mut out, "", "too_large", "the value is larger than 256 KiB");
    } else if depth(value) > MAX_VALUE_DEPTH {
        push(&mut out, "", "too_deep", "the value nests too deeply");
    }
    out
}

fn check_obj(rule: &ObjRule, v: &Value, path: &str, out: &mut Vec<Problem>) {
    let Some(o) = v.as_object() else {
        push(out, path, "type", "expected an object");
        return;
    };
    for name in &rule.required {
        if !o.contains_key(name) {
            let at = format!("{path}/{name}");
            push(out, &at, "required", "this setting is required");
        }
    }
    for (name, sub) in o {
        let at = format!("{path}/{name}");
        if let Some((_, node)) = rule.props.iter().find(|(n, _)| n == name) {
            check_node(node, sub, &at, out);
        } else {
            let msg = "this setting is not in the schema";
            push(out, &at, "unknown_property", msg);
        }
    }
}

fn check_node(node: &Node, v: &Value, path: &str, out: &mut Vec<Problem>) {
    match node {
        Node::Bool => {
            if !v.is_boolean() {
                push(out, path, "type", "expected true or false");
            }
        }
        Node::Num { integer, min, max } => check_num(*integer, *min, *max, v, path, out),
        Node::Str(rule) => check_str(rule, v, path, out),
        Node::List(rule) => {
            let Some(list) = v.as_array() else {
                push(out, path, "type", "expected a list");
                return;
            };
            if list.len() > MAX_ITEMS {
                push(out, path, "max_items", "a list holds 256 items at most");
                return;
            }
            for (i, item) in list.iter().enumerate() {
                let at = format!("{path}/{i}");
                check_str(rule, item, &at, out);
            }
        }
        Node::Obj(rule) => check_obj(rule, v, path, out),
    }
}

fn check_num(integer: bool, min: Option<f64>, max: Option<f64>, v: &Value, path: &str, out: &mut Vec<Problem>) {
    let number = v.as_number().filter(|n| !integer || is_integral(n));
    let Some(f) = number.and_then(serde_json::Number::as_f64) else {
        push(out, path, "type", "expected a number of this type");
        return;
    };
    if min.is_some_and(|m| f < m) {
        push(out, path, "minimum", "below the minimum");
    }
    if max.is_some_and(|m| f > m) {
        push(out, path, "maximum", "above the maximum");
    }
}

fn check_str(rule: &StrRule, v: &Value, path: &str, out: &mut Vec<Problem>) {
    let Some(s) = v.as_str() else {
        push(out, path, "type", "expected text");
        return;
    };
    if chars(s) > rule.max_chars {
        push(out, path, "max_length", "too long");
        return;
    }
    let lines = matches!(rule.format, Format::Multiline | Format::Markdown);
    let bad = |c: char| c.is_control() && !(lines && (c == '\n' || c == '\r' || c == '\t'));
    if s.chars().any(bad) {
        let msg = "control characters are not allowed";
        push(out, path, "control_chars", msg);
        return;
    }
    if rule.format == Format::Uri && !is_http_uri(s) {
        push(out, path, "uri", "expected an http or https address");
    }
    if rule.secret_ref && !is_secret_name(s) {
        let msg = "expected the name of a secret, like GH_TOKEN";
        push(out, path, "secret_name", msg);
    }
    if !rule.choices.is_empty() && !rule.choices.iter().any(|c| c == s) {
        push(out, path, "enum", "not one of the choices");
    }
    if rule.pattern.as_ref().is_some_and(|re| !re.is_match(s)) {
        push(out, path, "pattern", "does not match the expected form");
    }
}

/// Check `schema` against the dialect, then `value` against it.
pub fn validate(schema: &Value, value: &Value) -> Outcome {
    let mut out = Outcome::default();
    match compile(schema) {
        Ok(s) => out.value = s.check(value),
        Err(problems) => out.schema = problems,
    }
    out
}

/// [`validate`] for two JSON texts: sizes are checked before parsing.
pub fn validate_text(schema: &str, value: &str) -> Outcome {
    let mut out = Outcome::default();
    if schema.len() > MAX_SCHEMA_BYTES {
        let msg = "the schema is larger than 64 KiB";
        push(&mut out.schema, "", "too_large", msg);
        return out;
    }
    if value.len() > MAX_VALUE_BYTES {
        let msg = "the value is larger than 256 KiB";
        push(&mut out.value, "", "too_large", msg);
        return out;
    }
    let Ok(s) = serde_json::from_str::<Value>(schema) else {
        push(&mut out.schema, "", "json", "the schema is not JSON");
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(value) else {
        push(&mut out.value, "", "json", "the value is not JSON");
        return out;
    };
    validate(&s, &v)
}
