//! Evaluation of `cfg(...)` expressions against the exact set of `cfg`s
//! `rustc --print=cfg` reports for a target.
//!
//! Dependencies are gated with `[target.'cfg(unix)'.dependencies]`, and the
//! set of names that hold depends on the target *and* on `RUSTFLAGS`
//! (`-C target-feature=+crt-static` changes `target_feature`), so a built-in
//! table of known triples can't answer it. Asking `rustc` can, for built-in
//! and custom targets alike.

/// One `cfg` the compiler defines: `unix` or `target_os = "linux"`.
pub type Cfg = (String, Option<String>);

/// A parsed `cfg` predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfgExpr {
    Not(Box<CfgExpr>),
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Name(String),
    KeyValue(String, String),
}

impl CfgExpr {
    /// Parses the inside of `cfg(...)`, e.g. `all(unix, not(target_os = "macos"))`.
    pub fn parse(input: &str) -> Result<CfgExpr, String> {
        let mut p = Parser {
            chars: input.chars().collect(),
            pos: 0,
        };
        let expr = p.expr()?;
        p.skip_ws();
        if p.pos != p.chars.len() {
            return Err(format!("unexpected trailing input in cfg `{input}`"));
        }
        Ok(expr)
    }

    /// Evaluates against the compiler's cfg set.
    pub fn matches(&self, cfgs: &[Cfg]) -> bool {
        match self {
            CfgExpr::Not(e) => !e.matches(cfgs),
            CfgExpr::All(v) => v.iter().all(|e| e.matches(cfgs)),
            CfgExpr::Any(v) => v.iter().any(|e| e.matches(cfgs)),
            CfgExpr::Name(n) => cfgs.iter().any(|(k, v)| k == n && v.is_none()),
            CfgExpr::KeyValue(k, val) => cfgs
                .iter()
                .any(|(ck, cv)| ck == k && cv.as_deref() == Some(val.as_str())),
        }
    }
}

/// Whether a platform string from a manifest (`cfg(unix)` or a bare target
/// triple) applies to a target with this `name` and cfg set. A string that
/// fails to parse is treated as applying, like Cargo would rather include a
/// dependency than silently drop it.
pub fn platform_matches(platform: &str, target_name: &str, cfgs: &[Cfg]) -> bool {
    let platform = platform.trim();
    if platform.is_empty() {
        return true;
    }
    match platform
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => CfgExpr::parse(inner).map_or(true, |e| e.matches(cfgs)),
        None => platform == target_name,
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn skip_ws(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        self.skip_ws();
        let start = self.pos;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(format!("expected an identifier at position {start}"));
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.chars.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn list(&mut self) -> Result<Vec<CfgExpr>, String> {
        if !self.eat('(') {
            return Err("expected `(`".into());
        }
        let mut out = Vec::new();
        loop {
            if self.eat(')') {
                return Ok(out);
            }
            out.push(self.expr()?);
            if self.eat(',') {
                continue;
            }
            if self.eat(')') {
                return Ok(out);
            }
            return Err("expected `,` or `)`".into());
        }
    }

    fn expr(&mut self) -> Result<CfgExpr, String> {
        let name = self.ident()?;
        match name.as_str() {
            "not" => {
                let mut inner = self.list()?;
                if inner.len() != 1 {
                    return Err("`not` takes exactly one predicate".into());
                }
                Ok(CfgExpr::Not(Box::new(inner.remove(0))))
            }
            "all" => Ok(CfgExpr::All(self.list()?)),
            "any" => Ok(CfgExpr::Any(self.list()?)),
            _ => {
                if self.eat('=') {
                    self.skip_ws();
                    if !self.eat('"') {
                        return Err("expected a quoted value".into());
                    }
                    let start = self.pos;
                    while self.chars.get(self.pos).is_some_and(|c| *c != '"') {
                        self.pos += 1;
                    }
                    let value: String = self.chars[start..self.pos].iter().collect();
                    if !self.eat('"') {
                        return Err("unterminated string".into());
                    }
                    Ok(CfgExpr::KeyValue(name, value))
                } else {
                    Ok(CfgExpr::Name(name))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux() -> Vec<Cfg> {
        let kv = |k: &str, v: &str| (k.to_string(), Some(v.to_string()));
        vec![
            ("unix".to_string(), None),
            ("debug_assertions".to_string(), None),
            kv("target_os", "linux"),
            kv("target_arch", "x86_64"),
            kv("target_feature", "sse2"),
            kv("target_feature", "fxsr"),
            kv("target_pointer_width", "64"),
        ]
    }

    fn eval(s: &str) -> bool {
        platform_matches(s, "x86_64-unknown-linux-gnu", &linux())
    }

    #[test]
    fn names_key_values_and_combinators() {
        assert!(eval("cfg(unix)"));
        assert!(!eval("cfg(windows)"));
        assert!(eval("cfg(target_os = \"linux\")"));
        assert!(eval("cfg(target_feature = \"fxsr\")"));
        assert!(!eval("cfg(target_feature = \"avx2\")"));
        assert!(eval("cfg(all(unix, target_pointer_width = \"64\"))"));
        assert!(eval("cfg(any(windows, target_os = \"linux\"))"));
        assert!(eval("cfg(not(windows))"));
        assert!(!eval("cfg(all(unix, not(target_os = \"linux\")))"));
        assert!(eval("cfg( all( unix , target_arch=\"x86_64\" ) )"));
    }

    #[test]
    fn bare_triples_compare_by_name() {
        assert!(eval("x86_64-unknown-linux-gnu"));
        assert!(!eval("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn unparseable_expressions_apply_rather_than_drop_a_dependency() {
        assert!(eval("cfg(all(unix,)"));
        assert!(eval("cfg(??)"));
    }

    #[test]
    fn trailing_commas_and_nesting() {
        assert!(eval("cfg(all(unix, any(windows, target_os = \"linux\"),))"));
    }
}
