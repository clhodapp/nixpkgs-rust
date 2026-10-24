//! Regular expressions for `builtins.match` and `builtins.split`.
//!
//! Nix compiles patterns as POSIX extended regular expressions with the C++
//! standard library. Three behaviours of that implementation are kept here:
//! a backslash inside a bracket expression is a literal character, `.`
//! matches a newline, and a search reports the longest match at the
//! leftmost position while captures prefer earlier alternatives.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use regex::bytes::{Regex, RegexBuilder};
use regex_automata::hybrid::dfa::{Cache, DFA};
use regex_automata::{Anchored, Input, MatchKind};

use crate::error::{Error, R};

/// A compiled pattern.
pub struct NixRegex {
    /// The pattern anchored at both ends, for `match` and for captures.
    full: Regex,
    /// The unanchored pattern, to find where the leftmost match starts.
    search: Regex,
    /// A DFA reporting every match end, to find the longest match.
    longest: DFA,
    cache: RefCell<Cache>,
}

/// Rewrites a POSIX extended pattern into the syntax of the `regex` crate.
fn translate(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = String::with_capacity(p.len() + 8);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            out.push('\\');
            out.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        if c != b'[' {
            push_byte(&mut out, b, &mut i);
            continue;
        }
        // A bracket expression.
        out.push('[');
        i += 1;
        if i < b.len() && b[i] == b'^' {
            out.push('^');
            i += 1;
        }
        let mut first = true;
        while i < b.len() {
            let c = b[i];
            if c == b']' && !first {
                break;
            }
            first = false;
            if c == b'[' && i + 1 < b.len() && matches!(b[i + 1], b':' | b'=' | b'.') {
                let delim = b[i + 1];
                if let Some(end) = find_close(b, i + 2, delim) {
                    if delim == b':' {
                        out.push_str(&p[i..end + 2]);
                    } else {
                        for &x in &b[i + 2..end] {
                            escape_in_class(&mut out, x);
                        }
                    }
                    i = end + 2;
                    continue;
                }
            }
            if c == b'-' {
                let at_end = i + 1 < b.len() && b[i + 1] == b']';
                let after_open = out.ends_with('[') || out.ends_with("[^");
                if at_end || after_open {
                    out.push_str("\\-");
                } else {
                    out.push('-');
                }
                i += 1;
                continue;
            }
            if c < 0x80 {
                escape_in_class(&mut out, c);
                i += 1;
            } else {
                push_byte(&mut out, b, &mut i);
            }
        }
        out.push(']');
        i += 1;
    }
    out
}

fn find_close(b: &[u8], from: usize, delim: u8) -> Option<usize> {
    (from..b.len().saturating_sub(1)).find(|&j| b[j] == delim && b[j + 1] == b']')
}

fn escape_in_class(out: &mut String, c: u8) {
    match c {
        b'\\' | b'[' | b']' | b'&' | b'~' | b'^' => {
            out.push('\\');
            out.push(c as char);
        }
        _ => out.push(c as char),
    }
}

/// Copies one (possibly multi-byte) character.
fn push_byte(out: &mut String, b: &[u8], i: &mut usize) {
    let len = match b[*i] {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    };
    let end = (*i + len).min(b.len());
    out.push_str(&String::from_utf8_lossy(&b[*i..end]));
    *i = end;
}

fn build(pattern: &str) -> R<NixRegex> {
    let invalid = |_| Error::eval(format!("invalid regular expression '{pattern}'"));
    let t = translate(pattern);
    let mk = |s: &str| {
        RegexBuilder::new(s)
            .unicode(false)
            .dot_matches_new_line(true)
            .size_limit(64 << 20)
            .build()
    };
    let full = mk(&format!("^(?:{t})$")).map_err(invalid)?;
    let search = mk(&t).map_err(invalid)?;
    let longest = DFA::builder()
        .configure(DFA::config().match_kind(MatchKind::All))
        .syntax(
            regex_automata::util::syntax::Config::new()
                .unicode(false)
                .utf8(false)
                .dot_matches_new_line(true),
        )
        .build(&t)
        .map_err(|_| Error::eval(format!("invalid regular expression '{pattern}'")))?;
    let cache = RefCell::new(longest.create_cache());
    Ok(NixRegex {
        full,
        search,
        longest,
        cache,
    })
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<NixRegex>>> = RefCell::new(HashMap::new());
}

/// Compiles a pattern, reusing earlier compilations.
pub fn compile(pattern: &str) -> R<Rc<NixRegex>> {
    if let Some(r) = CACHE.with(|c| c.borrow().get(pattern).cloned()) {
        return Ok(r);
    }
    let r = Rc::new(build(pattern)?);
    CACHE.with(|c| c.borrow_mut().insert(pattern.to_string(), r.clone()));
    Ok(r)
}

/// The capture groups of a match, `None` for a group that did not take part.
pub type Groups = Vec<Option<Vec<u8>>>;

impl NixRegex {
    /// `builtins.match`: the groups when the whole string matches.
    pub fn match_full(&self, s: &[u8]) -> Option<Groups> {
        let caps = self.full.captures(s)?;
        Some(
            (1..caps.len())
                .map(|i| caps.get(i).map(|m| m.as_bytes().to_vec()))
                .collect(),
        )
    }

    fn longest_end(&self, s: &[u8], start: usize) -> usize {
        let input = Input::new(s).range(start..).anchored(Anchored::Yes);
        match self.longest.try_search_fwd(&mut self.cache.borrow_mut(), &input) {
            Ok(Some(m)) => m.offset(),
            _ => start,
        }
    }

    /// The next match at or after `from`: its span and groups.
    pub fn find_from(&self, s: &[u8], from: usize) -> Option<(usize, usize, Groups)> {
        let m = self.search.find_at(s, from)?;
        let start = m.start();
        let end = self.longest_end(s, start).max(m.end());
        let piece = &s[start..end];
        let groups = match self.full.captures(piece) {
            Some(caps) => (1..caps.len())
                .map(|i| caps.get(i).map(|m| m.as_bytes().to_vec()))
                .collect(),
            None => vec![None; self.full.captures_len() - 1],
        };
        Some((start, end, groups))
    }

    /// `builtins.split`: the text between matches, alternating with the
    /// groups of each match.
    pub fn split(&self, s: &[u8]) -> Vec<Result<Vec<u8>, Groups>> {
        let mut out = Vec::new();
        let mut last = 0;
        let mut pos = 0;
        while pos <= s.len() {
            let Some((start, end, groups)) = self.find_from(s, pos) else {
                break;
            };
            out.push(Ok(s[last..start].to_vec()));
            out.push(Err(groups));
            last = end;
            pos = if end == start { end + 1 } else { end };
        }
        out.push(Ok(s[last.min(s.len())..].to_vec()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> Option<Vec<Option<String>>> {
        compile(p).unwrap().match_full(s.as_bytes()).map(|g| {
            g.into_iter()
                .map(|x| x.map(|b| String::from_utf8(b).unwrap()))
                .collect()
        })
    }

    // Expected values observed from Nix 2.34.9.
    #[test]
    fn matches_like_nix() {
        assert_eq!(m("[\\.]", "\\"), Some(vec![]));
        assert_eq!(m("[\\.]", "."), Some(vec![]));
        assert_eq!(m("a.b", "a\nb"), Some(vec![]));
        assert_eq!(m("[]a]", "]"), Some(vec![]));
        assert_eq!(m("[a-]", "-"), Some(vec![]));
        assert_eq!(m("a|", ""), Some(vec![]));
        assert_eq!(
            m("(a|ab)(c|bcd)(d*)", "abcd"),
            Some(vec![Some("a".into()), Some("bcd".into()), Some("".into())])
        );
    }

    fn show(parts: Vec<Result<Vec<u8>, Groups>>) -> String {
        let mut s = String::new();
        for p in parts {
            match p {
                Ok(t) => s.push_str(&format!("{:?} ", String::from_utf8(t).unwrap())),
                Err(g) => {
                    s.push_str("[ ");
                    for x in g {
                        match x {
                            Some(t) => s.push_str(&format!("{:?} ", String::from_utf8(t).unwrap())),
                            None => s.push_str("null "),
                        }
                    }
                    s.push_str("] ");
                }
            }
        }
        s
    }

    #[test]
    fn splits_like_nix() {
        let sp = |p: &str, s: &str| show(compile(p).unwrap().split(s.as_bytes()));
        assert_eq!(sp("(a)|b", "xaybz"), r#""x" [ "a" ] "y" [ null ] "z" "#);
        assert_eq!(sp("a|ab", "xabx"), r#""x" [ ] "x" "#);
        assert_eq!(sp("x*", "abc"), r#""" [ ] "a" [ ] "b" [ ] "c" [ ] "" "#);
        assert_eq!(sp("(b)", "abc"), r#""a" [ "b" ] "c" "#);
    }
}
