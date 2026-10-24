//! Store paths, hashes and file-system archives.
//!
//! Evaluation never writes to the Nix store. It computes the paths that
//! sources, `builtins.toFile` texts and derivations would get, and records
//! what each of those paths holds in [`STATE`], so that the build step can
//! add exactly those objects to the store through the daemon.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;

use sha2::Digest;

use crate::error::{Error, R};
use crate::paths::base_name_of;
use crate::string::{Context, CtxElem};
use crate::value::Value;

/// The store directory, `/nix/store` unless `NIX_STORE_DIR` says otherwise.
pub fn store_dir() -> &'static str {
    use std::sync::OnceLock;
    static DIR: OnceLock<String> = OnceLock::new();
    DIR.get_or_init(|| std::env::var("NIX_STORE_DIR").unwrap_or_else(|_| "/nix/store".to_string()))
}

// ---------------------------------------------------------------------------
// Encodings

const NIX32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Encodes bytes in the base-32 variant Nix uses for store paths and hashes.
pub fn nix32_encode(h: &[u8]) -> String {
    if h.is_empty() {
        return String::new();
    }
    let len = (h.len() * 8 - 1) / 5 + 1;
    let mut s = String::with_capacity(len);
    for n in (0..len).rev() {
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        let mut c = (h[i] as u32) >> j;
        if i + 1 < h.len() {
            c |= (h[i + 1] as u32) << (8 - j);
        }
        s.push(NIX32[(c & 0x1f) as usize] as char);
    }
    s
}

/// Decodes the base-32 variant Nix uses; `size` is the number of bytes.
pub fn nix32_decode(s: &str, size: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; size];
    let bytes = s.as_bytes();
    if bytes.len() != (size * 8).div_ceil(5) {
        return None;
    }
    for (n, &ch) in bytes.iter().rev().enumerate() {
        let digit = NIX32.iter().position(|&c| c == ch)? as u32;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        out[i] |= (digit << j) as u8;
        let carry = digit >> (8 - j);
        if i + 1 < size {
            out[i + 1] |= carry as u8;
        } else if carry != 0 {
            return None;
        }
    }
    Some(out)
}

pub fn hex_encode(h: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(h.len() * 2);
    for b in h {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Hashes

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Algo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl Algo {
    pub fn parse(s: &str) -> Option<Algo> {
        Some(match s {
            "md5" => Algo::Md5,
            "sha1" => Algo::Sha1,
            "sha256" => Algo::Sha256,
            "sha512" => Algo::Sha512,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "md5",
            Algo::Sha1 => "sha1",
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
        }
    }

    pub fn size(self) -> usize {
        match self {
            Algo::Md5 => 16,
            Algo::Sha1 => 20,
            Algo::Sha256 => 32,
            Algo::Sha512 => 64,
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Algo::Md5 => md5::Md5::digest(data).to_vec(),
            Algo::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Algo::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Algo::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

/// A hash and its algorithm.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hash {
    pub algo: Algo,
    pub bytes: Vec<u8>,
}

/// How to print a hash.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HashFormat {
    Base16,
    Nix32,
    Base64,
    Sri,
}

impl Hash {
    pub fn of(algo: Algo, data: &[u8]) -> Hash {
        Hash {
            algo,
            bytes: algo.digest(data),
        }
    }

    pub fn sha256(data: &[u8]) -> Hash {
        Hash::of(Algo::Sha256, data)
    }

    pub fn hex(&self) -> String {
        hex_encode(&self.bytes)
    }

    pub fn format(&self, f: HashFormat, with_algo: bool) -> String {
        let body = match f {
            HashFormat::Base16 => hex_encode(&self.bytes),
            HashFormat::Nix32 => nix32_encode(&self.bytes),
            HashFormat::Base64 => base64_encode(&self.bytes),
            HashFormat::Sri => return format!("{}-{}", self.algo.name(), base64_encode(&self.bytes)),
        };
        if with_algo {
            format!("{}:{}", self.algo.name(), body)
        } else {
            body
        }
    }

    /// Parses a hash the way Nix's `Hash::parseAny` does: an SRI hash, an
    /// `algo:` prefixed hash, or a bare hash in base 16, Nix base 32 or base 64
    /// when the algorithm is known from elsewhere.
    pub fn parse(s: &str, algo: Option<Algo>) -> R<Hash> {
        let (algo, body, sri) = if let Some((a, rest)) = s.split_once(':') {
            let a = Algo::parse(a).ok_or_else(|| Error::eval(format!("unknown hash algorithm '{a}' in '{s}'")))?;
            (a, rest, false)
        } else if let Some((a, rest)) = s.split_once('-')
            && let Some(a) = Algo::parse(a)
        {
            (a, rest, true)
        } else {
            let a = algo.ok_or_else(|| {
                Error::eval(format!(
                    "hash '{s}' does not include a type, nor is the type otherwise known from context"
                ))
            })?;
            (a, s, false)
        };
        let size = algo.size();
        let bytes = if sri {
            base64_decode(body).filter(|b| b.len() == size)
        } else if body.len() == size * 2 {
            hex_decode(body)
        } else if body.len() == (size * 8).div_ceil(5) {
            nix32_decode(body, size)
        } else if body.len() == size.div_ceil(3) * 4 {
            base64_decode(body).filter(|b| b.len() == size)
        } else {
            None
        };
        match bytes {
            Some(bytes) => Ok(Hash { algo, bytes }),
            None => Err(Error::eval(format!(
                "invalid {} hash '{s}'",
                if sri { "SRI" } else { algo.name() }
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Store paths

/// Folds a hash to `size` bytes by XOR, as Nix's `compressHash` does.
pub fn compress_hash(h: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    for (i, b) in h.iter().enumerate() {
        out[i % size] ^= b;
    }
    out
}

/// `makeStorePath`: the path for an object of type `ty` whose fingerprint
/// hash is `hash`.
pub fn make_store_path(ty: &str, hash: &Hash, name: &str) -> String {
    let s = format!(
        "{ty}:{}:{}:{}:{name}",
        hash.algo.name(),
        hash.hex(),
        store_dir()
    );
    let h = Hash::sha256(s.as_bytes());
    format!("{}/{}-{name}", store_dir(), nix32_encode(&compress_hash(&h.bytes, 20)))
}

fn make_type(ty: &str, refs: &BTreeSet<String>, self_ref: bool) -> String {
    let mut s = ty.to_string();
    for r in refs {
        s.push(':');
        s.push_str(r);
    }
    if self_ref {
        s.push_str(":self");
    }
    s
}

/// The path of a `text`-hashed object: a `.drv` file or a `builtins.toFile` text.
pub fn make_text_path(name: &str, contents: &[u8], refs: &BTreeSet<String>) -> String {
    make_store_path(&make_type("text", refs, false), &Hash::sha256(contents), name)
}

/// The output path of an input-addressed derivation output.
pub fn make_output_path(id: &str, hash_modulo: &Hash, name: &str) -> String {
    let n = if id == "out" { name.to_string() } else { format!("{name}-{id}") };
    make_store_path(&format!("output:{id}"), hash_modulo, &n)
}

/// How a content-addressed object is hashed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// The bytes of a single file.
    Flat,
    /// The archive (NAR) of a file tree.
    Recursive,
    /// Text with references, as `.drv` files use.
    Text,
}

impl Method {
    /// The prefix of the method in a derivation's output hash algorithm (`r:`).
    pub fn prefix(self) -> &'static str {
        match self {
            Method::Flat => "",
            Method::Recursive => "r:",
            Method::Text => "text:",
        }
    }
}

/// `makeFixedOutputPath`.
pub fn make_fixed_output_path(method: Method, hash: &Hash, name: &str, refs: &BTreeSet<String>) -> String {
    match method {
        Method::Recursive if hash.algo == Algo::Sha256 => make_store_path(&make_type("source", refs, false), hash, name),
        Method::Text => make_store_path(&make_type("text", refs, false), hash, name),
        _ => {
            let inner = format!("fixed:out:{}{}:{}:", method.prefix(), hash.algo.name(), hash.hex());
            make_store_path("output:out", &Hash::sha256(inner.as_bytes()), name)
        }
    }
}

/// Checks a store path name the way Nix does.
pub fn check_name(name: &str) -> R<()> {
    if name.is_empty() {
        return Err(Error::eval("store path name is empty"));
    }
    if name.len() > 211 {
        return Err(Error::eval(format!("store path name '{name}' is longer than 211 characters")));
    }
    if name.starts_with('.') {
        return Err(Error::eval(format!("store path name '{name}' starts with a period")));
    }
    for c in name.bytes() {
        if !(c.is_ascii_alphanumeric() || b"+-._?=".contains(&c)) {
            return Err(Error::eval(format!(
                "store path name '{name}' contains illegal character '{}'",
                c as char
            )));
        }
    }
    Ok(())
}

/// Whether `p` is a path directly inside the store (not a file within a store object).
pub fn is_store_path(p: &str) -> bool {
    match p.strip_prefix(store_dir()).and_then(|r| r.strip_prefix('/')) {
        Some(base) => base.len() > 33 && !base.contains('/') && base.as_bytes()[32] == b'-',
        None => false,
    }
}

/// The store object a path is in: `/nix/store/<hash>-<name>` for
/// `/nix/store/<hash>-<name>/sub/path`.
pub fn to_store_path(p: &str) -> Option<&str> {
    let rest = p.strip_prefix(store_dir())?.strip_prefix('/')?;
    let end = rest.find('/').unwrap_or(rest.len());
    let base = &rest[..end];
    if base.len() > 33 && base.as_bytes()[32] == b'-' {
        Some(&p[..store_dir().len() + 1 + end])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Archives

/// Receives a NAR as it is produced.
pub trait NarSink {
    fn write(&mut self, data: &[u8]);
}

impl NarSink for Vec<u8> {
    fn write(&mut self, data: &[u8]) {
        self.extend_from_slice(data);
    }
}

impl NarSink for sha2::Sha256 {
    fn write(&mut self, data: &[u8]) {
        self.update(data);
    }
}

/// A NAR sink that hashes and also counts bytes.
pub struct HashingSink {
    pub hasher: sha2::Sha256,
    pub len: u64,
}

impl HashingSink {
    pub fn new() -> Self {
        HashingSink {
            hasher: sha2::Sha256::new(),
            len: 0,
        }
    }
}

impl Default for HashingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl NarSink for HashingSink {
    fn write(&mut self, data: &[u8]) {
        self.len += data.len() as u64;
        self.hasher.update(data);
    }
}

fn nar_str(sink: &mut dyn NarSink, s: &[u8]) {
    sink.write(&(s.len() as u64).to_le_bytes());
    sink.write(s);
    let pad = (8 - s.len() % 8) % 8;
    if pad > 0 {
        sink.write(&[0u8; 8][..pad]);
    }
}

/// Decides whether an entry below the root of a dumped tree is included. It
/// receives the entry's path and its type (`regular`, `directory`,
/// `symlink`, `unknown`).
pub type Filter<'a> = &'a mut dyn FnMut(&str, &str) -> R<bool>;

/// Writes the NAR serialisation of `path`.
pub fn dump_path(path: &Path, sink: &mut dyn NarSink, filter: Option<Filter>) -> R<()> {
    nar_str(sink, b"nix-archive-1");
    let mut filter = filter;
    dump_rec(path, sink, &mut filter)
}

fn io_err(p: &Path, e: std::io::Error) -> Box<Error> {
    Error::eval(format!("cannot read '{}': {e}", p.display()))
}

fn file_type_name(m: &fs::Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "directory"
    } else if t.is_file() {
        "regular"
    } else {
        "unknown"
    }
}

fn dump_rec(path: &Path, sink: &mut dyn NarSink, filter: &mut Option<Filter>) -> R<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_err(path, e))?;
    nar_str(sink, b"(");
    let t = meta.file_type();
    if t.is_symlink() {
        nar_str(sink, b"type");
        nar_str(sink, b"symlink");
        nar_str(sink, b"target");
        let target = fs::read_link(path).map_err(|e| io_err(path, e))?;
        nar_str(sink, target.as_os_str().as_encoded_bytes());
    } else if t.is_file() {
        nar_str(sink, b"type");
        nar_str(sink, b"regular");
        if meta.permissions().mode() & 0o100 != 0 {
            nar_str(sink, b"executable");
            nar_str(sink, b"");
        }
        nar_str(sink, b"contents");
        let data = fs::read(path).map_err(|e| io_err(path, e))?;
        nar_str(sink, &data);
    } else if t.is_dir() {
        nar_str(sink, b"type");
        nar_str(sink, b"directory");
        let mut entries: Vec<_> = fs::read_dir(path)
            .map_err(|e| io_err(path, e))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| io_err(path, e))?;
        entries.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
        for name in entries {
            let child = path.join(&name);
            if let Some(f) = filter.as_mut() {
                let m = fs::symlink_metadata(&child).map_err(|e| io_err(&child, e))?;
                if !f(&child.to_string_lossy(), file_type_name(&m))? {
                    continue;
                }
            }
            nar_str(sink, b"entry");
            nar_str(sink, b"(");
            nar_str(sink, b"name");
            nar_str(sink, name.as_encoded_bytes());
            nar_str(sink, b"node");
            dump_rec(&child, sink, filter)?;
            nar_str(sink, b")");
        }
    } else {
        return Err(Error::eval(format!("file '{}' has an unsupported type", path.display())));
    }
    nar_str(sink, b")");
    Ok(())
}

/// The SHA-256 of the NAR of `path`, and the NAR's size.
pub fn nar_hash(path: &Path, filter: Option<Filter>) -> R<(Hash, u64)> {
    let mut sink = HashingSink::new();
    dump_path(path, &mut sink, filter)?;
    Ok((
        Hash {
            algo: Algo::Sha256,
            bytes: sink.hasher.finalize().to_vec(),
        },
        sink.len,
    ))
}

// ---------------------------------------------------------------------------
// Objects the evaluation would add to the store

/// A source tree copied into the store.
pub struct Source {
    /// Where the tree is on disk.
    pub fs_path: String,
    /// How it is hashed: a NAR, or the bytes of a single file.
    pub method: Method,
    /// The filter `builtins.path` or `filterSource` applied, if any.
    pub filter: Option<Value>,
}

/// Everything evaluation has decided the store should contain.
#[derive(Default)]
pub struct StoreState {
    /// Copies of source trees, by store path.
    pub sources: HashMap<String, Source>,
    /// `builtins.toFile` texts, by store path: their contents and references.
    pub texts: HashMap<String, (Vec<u8>, BTreeSet<String>)>,
    /// Derivations, by `.drv` path.
    pub drvs: HashMap<String, Rc<crate::derivation::Derivation>>,
    /// The store path of each unfiltered source path already copied.
    pub copied: HashMap<String, String>,
    /// `hashDerivationModulo` results, by `.drv` path.
    pub drv_hashes: HashMap<String, crate::derivation::DrvHash>,
}

thread_local! {
    pub static STATE: RefCell<StoreState> = RefCell::new(StoreState::default());
}

/// Copies a path into the store, as interpolating a path into a string does,
/// and adds the result to `ctx`. Returns the store path.
pub fn copy_path_to_store(path: &str, ctx: &mut Context) -> R<Rc<str>> {
    if path.ends_with(".drv") {
        return Err(Error::eval("file names are not allowed to end in '.drv'"));
    }
    if let Some(sp) = STATE.with(|s| s.borrow().copied.get(path).cloned()) {
        let sp: Rc<str> = Rc::from(sp);
        ctx.insert(CtxElem::Opaque(sp.clone()));
        return Ok(sp);
    }
    let name = base_name_of(path).to_string();
    check_name(&name)?;
    let resolved = resolve_ancestors(path);
    let (hash, _) = nar_hash(Path::new(&resolved), None)?;
    let sp = make_fixed_output_path(Method::Recursive, &hash, &name, &BTreeSet::new());
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.copied.insert(path.to_string(), sp.clone());
        s.sources.insert(
            sp.clone(),
            Source {
                fs_path: resolved,
                method: Method::Recursive,
                filter: None,
            },
        );
    });
    let sp: Rc<str> = Rc::from(sp);
    ctx.insert(CtxElem::Opaque(sp.clone()));
    Ok(sp)
}

/// Resolves symbolic links in the directories leading to `path`, but not in
/// its last component.
pub fn resolve_ancestors(path: &str) -> String {
    let p = Path::new(path);
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(file)) => match fs::canonicalize(parent) {
            Ok(dir) => dir.join(file).to_string_lossy().into_owned(),
            Err(_) => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// Records a `builtins.toFile` text and returns its store path.
pub fn add_text(name: &str, contents: &[u8], refs: BTreeSet<String>) -> R<String> {
    check_name(name)?;
    let sp = make_text_path(name, contents, &refs);
    STATE.with(|s| s.borrow_mut().texts.insert(sp.clone(), (contents.to_vec(), refs)));
    Ok(sp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nix32_round_trip() {
        let h = Hash::sha256(b"hello");
        let s = nix32_encode(&h.bytes);
        assert_eq!(s.len(), 52);
        assert_eq!(nix32_decode(&s, 32).unwrap(), h.bytes);
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..10 {
            let data: Vec<u8> = (0..n).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
    }

    #[test]
    fn text_path_matches_nix() {
        // `builtins.toFile "foo" "bar"` in Nix 2.34.9.
        let p = make_text_path("foo", b"bar", &BTreeSet::new());
        assert_eq!(p, "/nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo");
    }

    #[test]
    fn hash_encodings_match_nix() {
        // `builtins.hashString "sha256" "abc"` and `builtins.convertHash` in Nix 2.34.9.
        let h = Hash::sha256(b"abc");
        assert_eq!(h.hex(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let sri = Hash::parse("sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=", None).unwrap();
        assert_eq!(
            sri.format(HashFormat::Nix32, false),
            "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
        );
    }
}
