//! Lexical path operations with the semantics of Nix's `canonPath`.

/// Makes an absolute path canonical without touching the file system:
/// collapses repeated slashes, drops `.` components, resolves `..` against
/// the preceding component (stopping at the root), and drops a trailing
/// slash.
pub fn canon_path(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    if parts.is_empty() {
        return "/".to_string();
    }
    let mut out = String::with_capacity(p.len());
    for c in parts {
        out.push('/');
        out.push_str(c);
    }
    out
}

/// `builtins.dirOf` on a path or string: everything before the last slash,
/// or `.` when there is no slash. A trailing slash counts as the last slash.
pub fn dir_of(p: &str) -> String {
    match p.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// `builtins.baseNameOf`: the last component, ignoring one trailing slash.
pub fn base_name_of(p: &str) -> &str {
    if p.is_empty() {
        return "";
    }
    let end = if p.ends_with('/') { p.len() - 1 } else { p.len() };
    let s = &p[..end];
    match s.rfind('/') {
        None => s,
        Some(i) => &s[i + 1..],
    }
}

/// Joins a path literal onto the directory of the file it appears in.
pub fn join(dir: &str, rel: &str) -> String {
    if rel.starts_with('/') {
        canon_path(rel)
    } else {
        canon_path(&format!("{dir}/{rel}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon() {
        assert_eq!(canon_path("/a/b/../c/./d//"), "/a/c/d");
        assert_eq!(canon_path("/.."), "/");
        assert_eq!(canon_path("/"), "/");
    }

    #[test]
    fn dir_and_base() {
        assert_eq!(dir_of("/a/b"), "/a");
        assert_eq!(dir_of("/a"), "/");
        assert_eq!(dir_of("a"), ".");
        assert_eq!(dir_of("a/b/"), "a/b");
        assert_eq!(dir_of("//a"), "/");
        assert_eq!(base_name_of("/a/b/"), "b");
        assert_eq!(base_name_of("/a/b"), "b");
    }
}
