// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A WKT text parsed once (#1058 step 3): the read helpers that each parsed the text they were
//! given ([`crate::declared_epsg`], [`crate::fits::Datums::of`], `Def::wkt_conversion`, …) read
//! the tree kept for it here. One identification parsed the same text 10 to 14 times.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, PoisonError};

use crate::wkt1;

/// A tree kept for its text; `None` for a text that is no WKT.
type Kept = (Box<str>, Option<Arc<wkt1::Node>>);

/// The trees parsed, by the hash of their text (taken before the lock) and then the text, with the
/// bytes they count against the memo's bound. A text that is no WKT is kept too, as `None`.
#[derive(Default)]
struct Memo {
    trees: HashMap<u64, Vec<Kept>>,
    bytes: usize,
}

/// The memo's bound: 1 MiB, counting each text and the memory its tree takes ([`weight`]).
const BOUND: usize = 1 << 20;

fn memo() -> &'static Mutex<Memo> {
    crate::host::before_lock();
    static M: std::sync::OnceLock<Mutex<Memo>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// The memory a tree takes: its nodes and arguments, and their strings' bytes. A tree weighs about
/// ten times its text, and up to about fifty for text of many short arguments (`A[1,1,1,...]`).
fn weight(n: &wkt1::Node) -> usize {
    let args = n.args.capacity() * std::mem::size_of::<wkt1::Arg>();
    n.key.capacity()
        + args
        + n.args
            .iter()
            .map(|a| match a {
                wkt1::Arg::Str(s) | wkt1::Arg::Ident(s) => s.capacity(),
                wkt1::Arg::Num(_) => 0,
                wkt1::Arg::Node(c) => weight(c),
            })
            .sum::<usize>()
}

/// The tree of a WKT text, as [`wkt1::parse`] reads it, parsed once while the memo keeps it (two
/// threads that miss it together each parse it, and keep the first).
///
/// The memo holds up to 1 MiB of texts and trees; when full, it is emptied and fills again. A
/// text whose tree alone weighs more is parsed each time. A parse depends on the text alone, so
/// nothing else empties it.
pub(crate) fn tree(text: &str) -> Option<Arc<wkt1::Node>> {
    let hash = crate::def::memo_keys().hash_one(text);
    {
        let m = memo().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, t)) = m.trees.get(&hash).and_then(|v| v.iter().find(|(k, _)| &**k == text)) {
            return t.clone();
        }
    }
    // Parsed without the lock.
    let t = wkt1::parse(text).map(Arc::new);
    let size = text.len().saturating_add(t.as_deref().map_or(0, weight));
    if size > BOUND {
        return t;
    }
    // The trees forgotten are dropped after the lock is released.
    let mut forgotten = None;
    {
        let mut m = memo().lock().unwrap_or_else(PoisonError::into_inner);
        if m.bytes + size > BOUND {
            forgotten = Some(std::mem::take(&mut *m));
        }
        let kept = m.trees.entry(hash).or_default();
        if !kept.iter().any(|(k, _)| &**k == text) {
            kept.push((text.into(), t.clone()));
            m.bytes += size;
        }
    }
    drop(forgotten);
    t
}

/// Holds the memo of trees across a fork, for [`crate::host::hold_for_fork`], with its hash keys
/// drawn first.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    crate::def::memo_keys();
    held.push(Box::new(memo().lock().unwrap_or_else(PoisonError::into_inner)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A text is parsed as `wkt1::parse` reads it, and the same tree is given again (the memo may
    /// have been emptied meanwhile by other tests); a text that is no WKT is none.
    #[test]
    fn a_text_is_parsed_once() {
        let text = r#"GEOGCS["parsed once",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        let first = tree(text).unwrap();
        assert_eq!(*first, wkt1::parse(text).unwrap());
        assert_eq!(*first, *tree(text).unwrap());
        assert!(tree("not WKT[").is_none());
    }

    /// A tree is weighed by the memory it takes, not its text's length: of many short arguments,
    /// a text within the parser's limit weighs more than the bound, and is parsed each time, not
    /// kept.
    #[test]
    fn a_tree_heavier_than_the_bound_is_not_kept() {
        let text = format!("GEOGCS[{}1]", "1,".repeat(wkt1::MAX_LEN / 2 - 10));
        let parsed = wkt1::parse(&text).unwrap();
        assert!(text.len() < BOUND / 4 && weight(&parsed) > BOUND, "{}", weight(&parsed));
        let first = tree(&text).unwrap();
        assert_eq!(*first, parsed);
        assert!(!Arc::ptr_eq(&first, &tree(&text).unwrap()));
    }

    /// The memo's lock is one `host::hold_for_fork` takes across a fork.
    #[cfg(all(unix, not(target_family = "wasm")))]
    #[test]
    fn hold_for_fork_takes_the_tree_memo_lock() {
        crate::host::assert_held_for_fork("trees", |wait| {
            let _g = memo().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
    }
}
