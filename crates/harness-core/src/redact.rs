//! Secret redaction. Every event passes through a [`Redactor`] before it is written.

use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde_json::Value;

/// Replacement text for redacted material.
pub const REDACTED: &str = "[REDACTED]";

/// Minimum length for a literal secret to be registered (shorter values would mangle text).
const MIN_LITERAL_LEN: usize = 6;

struct Patterns {
    /// `sk-...` and `sk-proj-...` style keys.
    sk: Regex,
    /// Google API keys.
    aiza: Regex,
    /// `Bearer <token>` header values.
    bearer: Regex,
    /// `api_key=...`, `password: ...` style assignments.
    assignment: Regex,
    /// JSON object keys whose string values are always secrets.
    secret_key: Regex,
}

fn patterns() -> Option<&'static Patterns> {
    static PATTERNS: OnceLock<Option<Patterns>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            Some(Patterns {
                sk: Regex::new(r"sk-[A-Za-z0-9_\-]{12,}").ok()?,
                aiza: Regex::new(r"AIza[0-9A-Za-z_\-]{30,}").ok()?,
                bearer: Regex::new(r"(?i)\bBearer\s+([A-Za-z0-9._~+/=\-]{8,})").ok()?,
                assignment: Regex::new(
                    r#"(?i)((?:api[_-]?key|secret(?:[_-]?key)?|access[_-]?token|auth[_-]?token|password|passwd)["']?\s*[=:]\s*["']?)([^\s"',;&]{6,})"#,
                )
                .ok()?,
                secret_key: Regex::new(
                    r"(?i)^(?:api[_-]?key|secret(?:[_-]?key)?|password|passwd|authorization|access[_-]?token|auth[_-]?token|token)$",
                )
                .ok()?,
            })
        })
        .as_ref()
}

/// Redacts literal secrets and common credential shapes.
#[derive(Clone, Default)]
pub struct Redactor {
    /// Literal secrets, longest first so overlapping values are fully removed.
    secrets: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("literal_secrets", &self.secrets.len())
            .finish()
    }
}

impl Redactor {
    /// Register literal secret values. Values shorter than six characters are ignored.
    pub fn new(secrets: Vec<String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|s| s.chars().count() >= MIN_LITERAL_LEN)
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    /// Register the current values of the named environment variables (missing ones are skipped).
    pub fn from_env_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::new(
            names
                .into_iter()
                .filter_map(|n| std::env::var(n.as_ref()).ok())
                .collect(),
        )
    }

    /// Add one more literal secret (ignored when shorter than six characters).
    pub fn add_secret(&mut self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.chars().count() >= MIN_LITERAL_LEN && !self.secrets.contains(&secret) {
            self.secrets.push(secret);
            self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    /// Whether `text` contains something that redaction would change.
    pub fn looks_like_secret(&self, text: &str) -> bool {
        self.redact_str(text) != text
    }

    /// Redact literal secrets and credential shapes in a string.
    pub fn redact_str(&self, text: &str) -> String {
        let mut out = Cow::Borrowed(text);
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
            }
        }
        if let Some(p) = patterns() {
            out = replace(out, &p.sk, |_| REDACTED.to_owned());
            out = replace(out, &p.aiza, |_| REDACTED.to_owned());
            out = replace(out, &p.bearer, |caps| {
                // Avoid mangling prose such as "Bearer authorization": require a token-like value.
                let tok = &caps[1];
                let tokenish = tok.len() >= 20
                    || tok
                        .chars()
                        .any(|c| c.is_ascii_digit() || "._~+/=-".contains(c));
                if tokenish {
                    REDACTED.to_owned()
                } else {
                    caps[0].to_owned()
                }
            });
            out = replace(out, &p.assignment, |caps| format!("{}{REDACTED}", &caps[1]));
        }
        out.into_owned()
    }

    /// Redact every string (keys' values, array items, nested objects) in place.
    ///
    /// String values under keys such as `api_key`, `password` or `authorization` are replaced
    /// wholesale regardless of shape.
    pub fn redact_json(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                let redacted = self.redact_str(s);
                if redacted != *s {
                    *s = redacted;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.redact_json(v)),
            Value::Object(map) => {
                let secret_key = patterns().map(|p| &p.secret_key);
                for (key, v) in map.iter_mut() {
                    let is_secret_key = secret_key.is_some_and(|re| re.is_match(key));
                    match v {
                        Value::String(s) if is_secret_key && !s.is_empty() => {
                            *s = REDACTED.to_owned();
                        }
                        _ => self.redact_json(v),
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

fn replace<'a>(
    text: Cow<'a, str>,
    re: &Regex,
    f: impl Fn(&Captures<'_>) -> String,
) -> Cow<'a, str> {
    if re.is_match(&text) {
        Cow::Owned(
            re.replace_all(&text, |caps: &Captures<'_>| f(caps))
                .into_owned(),
        )
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patterns_compile() {
        assert!(patterns().is_some());
    }

    #[test]
    fn redacts_literal_secret_anywhere() {
        let r = Redactor::new(vec!["hunter2-secret".into()]);
        assert_eq!(
            r.redact_str("use hunter2-secret here and hunter2-secret there"),
            "use [REDACTED] here and [REDACTED] there"
        );
    }

    #[test]
    fn ignores_short_literals() {
        let r = Redactor::new(vec!["abc".into(), "12345".into()]);
        assert_eq!(r.redact_str("abc 12345"), "abc 12345");
    }

    #[test]
    fn redacts_key_shapes() {
        let r = Redactor::default();
        let cases = [
            "sk-abcdefghijklmnopqrstuvwxyz012345",
            "sk-proj-Abc123_def456-ghi789JKL",
            "AIzaSyA1234567890abcdefghijklmnopqrstuv",
        ];
        for case in cases {
            let out = r.redact_str(&format!("key={case} end"));
            assert!(!out.contains(case), "{out}");
            assert!(out.contains(REDACTED), "{out}");
        }
        assert_eq!(
            r.redact_str("Authorization: Bearer abcDEF123456.tokenvalue"),
            "Authorization: [REDACTED]"
        );
        assert_eq!(r.redact_str("api_key=abcdef123456"), "api_key=[REDACTED]");
        assert_eq!(
            r.redact_str("\"password\": \"s3cr3tpass\""),
            "\"password\": \"[REDACTED]\""
        );
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let r = Redactor::default();
        for text in [
            "Bearer authentication is described here",
            "max_output_tokens: 2048",
            "the sky is blue",
            "OPENAI_API_KEY",
        ] {
            assert_eq!(r.redact_str(text), text);
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let r = Redactor::new(vec!["hunter2-secret".into()]);
        let once = r.redact_str("api_key=abcdef123456 hunter2-secret Bearer abcDEF123456xyz");
        assert_eq!(r.redact_str(&once), once);
    }

    #[test]
    fn redacts_nested_json_and_secret_keys() {
        let r = Redactor::new(vec!["hunter2-secret".into()]);
        let mut v = json!({
            "a": ["x hunter2-secret y", {"deep": "sk-abcdefghijklmnopqrstuv"}],
            "password": "anything",
            "n": 5,
            "ok": "fine"
        });
        r.redact_json(&mut v);
        assert_eq!(
            v,
            json!({
                "a": ["x [REDACTED] y", {"deep": "[REDACTED]"}],
                "password": "[REDACTED]",
                "n": 5,
                "ok": "fine"
            })
        );
    }

    #[test]
    fn debug_does_not_leak() {
        let r = Redactor::new(vec!["hunter2-secret".into()]);
        assert!(!format!("{r:?}").contains("hunter2"));
    }
}
