//! Attribute sets.

use std::rc::Rc;

use crate::symbol::{Sym, name};
use crate::value::{Pos, Value};

/// One attribute: its name, its (possibly unevaluated) value, and where it
/// was defined.
#[derive(Clone)]
pub struct Attr {
    pub k: Sym,
    pub v: Value,
    pub pos: Pos,
}

/// An attribute set, sorted by symbol id. Lookups are binary searches on the
/// id; operations that expose an order (`attrNames`, `attrValues`, the
/// environment of a derivation) sort by name with [`Attrs::by_name`].
#[derive(Clone, Default)]
pub struct Attrs {
    e: Vec<Attr>,
}

impl Attrs {
    pub fn new() -> Attrs {
        Attrs { e: Vec::new() }
    }

    /// Builds a set from entries the caller has already sorted by id, with
    /// no duplicates. The compiler emits static attribute sets this way.
    pub fn from_sorted(e: Vec<Attr>) -> Attrs {
        debug_assert!(e.windows(2).all(|w| w[0].k < w[1].k));
        Attrs { e }
    }

    /// Builds a set from entries in any order. A later entry with the same
    /// name replaces an earlier one, which is what `listToAttrs` needs
    /// reversed; callers that must reject duplicates use [`Attrs::insert_new`].
    pub fn from_vec_last_wins(mut e: Vec<Attr>) -> Attrs {
        e.reverse();
        e.sort_by_key(|a| a.k);
        e.dedup_by_key(|a| a.k);
        Attrs { e }
    }

    /// Builds a set where the first entry with a name wins.
    pub fn from_vec_first_wins(mut e: Vec<Attr>) -> Attrs {
        e.sort_by_key(|a| a.k);
        e.dedup_by_key(|a| a.k);
        Attrs { e }
    }

    pub fn len(&self) -> usize {
        self.e.len()
    }

    pub fn is_empty(&self) -> bool {
        self.e.is_empty()
    }

    fn find(&self, k: Sym) -> Result<usize, usize> {
        self.e.binary_search_by_key(&k, |a| a.k)
    }

    #[inline]
    pub fn get(&self, k: Sym) -> Option<&Value> {
        self.find(k).ok().map(|i| &self.e[i].v)
    }

    pub fn get_attr(&self, k: Sym) -> Option<&Attr> {
        self.find(k).ok().map(|i| &self.e[i])
    }

    pub fn contains(&self, k: Sym) -> bool {
        self.find(k).is_ok()
    }

    /// Inserts an attribute that must not exist yet. Returns false, leaving
    /// the set unchanged, if it does.
    pub fn insert_new(&mut self, a: Attr) -> bool {
        match self.find(a.k) {
            Ok(_) => false,
            Err(i) => {
                self.e.insert(i, a);
                true
            }
        }
    }

    /// Inserts or replaces an attribute.
    pub fn insert(&mut self, a: Attr) {
        match self.find(a.k) {
            Ok(i) => self.e[i] = a,
            Err(i) => self.e.insert(i, a),
        }
    }

    pub fn remove(&mut self, k: Sym) {
        if let Ok(i) = self.find(k) {
            self.e.remove(i);
        }
    }

    /// Entries in id order.
    pub fn iter(&self) -> std::slice::Iter<'_, Attr> {
        self.e.iter()
    }

    /// Entries in the order of their names' bytes, the order Nix exposes.
    pub fn by_name(&self) -> Vec<&Attr> {
        let mut v: Vec<(Rc<str>, &Attr)> = self.e.iter().map(|a| (name(a.k), a)).collect();
        v.sort_by(|x, y| x.0.as_bytes().cmp(y.0.as_bytes()));
        v.into_iter().map(|(_, a)| a).collect()
    }

    /// `a // b`: the attributes of both, `b` winning on a clash.
    pub fn update(a: &Attrs, b: &Attrs) -> Attrs {
        if b.e.len() * 8 < a.e.len() {
            return Attrs::update_small(a, b);
        }
        let mut e = Vec::with_capacity(a.e.len() + b.e.len());
        let (mut i, mut j) = (0, 0);
        while i < a.e.len() && j < b.e.len() {
            let (x, y) = (&a.e[i], &b.e[j]);
            if x.k < y.k {
                e.push(x.clone());
                i += 1;
            } else if y.k < x.k {
                e.push(y.clone());
                j += 1;
            } else {
                e.push(y.clone());
                i += 1;
                j += 1;
            }
        }
        e.extend_from_slice(&a.e[i..]);
        e.extend_from_slice(&b.e[j..]);
        Attrs { e }
    }

    /// `a // b` for a `b` much smaller than `a`: each entry of `b` is placed
    /// by binary search, and the entries of `a` between them are copied as
    /// runs instead of compared one by one.
    fn update_small(a: &Attrs, b: &Attrs) -> Attrs {
        let mut e = Vec::with_capacity(a.e.len() + b.e.len());
        let mut cur = 0;
        for y in &b.e {
            let rest = &a.e[cur..];
            match rest.binary_search_by_key(&y.k, |x| x.k) {
                Ok(i) => {
                    e.extend_from_slice(&rest[..i]);
                    e.push(y.clone());
                    cur += i + 1;
                }
                Err(i) => {
                    e.extend_from_slice(&rest[..i]);
                    e.push(y.clone());
                    cur += i;
                }
            }
        }
        e.extend_from_slice(&a.e[cur..]);
        Attrs { e }
    }

    pub fn into_vec(self) -> Vec<Attr> {
        self.e
    }

    pub fn as_slice(&self) -> &[Attr] {
        &self.e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[u32], tag: i64) -> Attrs {
        Attrs::from_sorted(
            keys.iter()
                .map(|&k| Attr {
                    k: Sym(k),
                    v: Value::Int(tag * 1000 + k as i64),
                    pos: Pos::default(),
                })
                .collect(),
        )
    }

    fn entries(a: &Attrs) -> Vec<(u32, i64)> {
        a.iter()
            .map(|x| match x.v {
                Value::Int(i) => (x.k.0, i),
                _ => unreachable!(),
            })
            .collect()
    }

    /// The general merge, which `update` uses when neither side is small.
    fn merged(a: &Attrs, b: &Attrs) -> Vec<(u32, i64)> {
        let mut m: std::collections::BTreeMap<u32, i64> = entries(a).into_iter().collect();
        m.extend(entries(b));
        m.into_iter().collect()
    }

    #[test]
    fn small_update_matches_the_merge() {
        // A pseudo-random walk over key sets, so both small and general
        // updates see overlaps, gaps and keys before and after `a`.
        let mut seed: u64 = 7;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for _ in 0..200 {
            let mut ak: Vec<u32> = (0..(next() % 200)).map(|_| next() % 400).collect();
            ak.sort();
            ak.dedup();
            let mut bk: Vec<u32> = (0..(next() % 12)).map(|_| next() % 420).collect();
            bk.sort();
            bk.dedup();
            let (a, b) = (set(&ak, 1), set(&bk, 2));
            assert_eq!(entries(&Attrs::update(&a, &b)), merged(&a, &b));
        }
    }
}
