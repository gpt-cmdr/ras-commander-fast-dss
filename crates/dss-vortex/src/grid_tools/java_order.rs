//! The order `java.util.HashSet` iterates pathnames in.
//!
//! `DssDataReader.getVariables` returns `new HashSet<>(griddedRecords)`, and `Normalizer.readAll`
//! iterates it to build the list `Normalizer.normalize` sums in `f32`. A float sum depends on its
//! order, so reproducing Vortex's bits needs Vortex's order: the set's, not time order.
//!
//! Targets JDK 21 (Vortex 1.1.0, D41). Clean-room public-API experiments on JDK 17
//! and 21 established the spread, sizing and insertion rules (P59/P59b reports).
//! The initial table is the smallest power of two >= 16 holding the collection length
//! at load 0.75, including duplicates. Bins iterate by index, then insertion order.
//! Insertion of a ninth distinct name into a bin grows tables below 64 bins. At 64
//! or more, that insertion may restructure the bin; the returned flag reports this
//! unsupported ordering. No JDK implementation source was consulted.

/// `String.hashCode` as its Javadoc defines it: `s[0]*31^(n-1) + s[1]*31^(n-2) + ... + s[n-1]` over
/// the UTF-16 code units, in wrapping 32-bit arithmetic.
pub fn java_string_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |h, unit| {
        h.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}

// The hash bits that choose a bin: the hash with its high half folded into its low half
// (black-box measured; see the module comment).
fn spread(h: i32) -> u32 {
    let h = h as u32;
    h ^ (h >> 16)
}

// Measured JDK 21 collection-constructor sizing, including duplicate inputs.
fn table_size(size: usize) -> usize {
    let mut table = 16;
    while table * 3 < size * 4 {
        table *= 2;
    }
    table
}

/// Names in JDK 21 HashSet iteration order; true means a tree bin may change that order.
/// Small-table collision-driven growth is reproduced; tree-bin ordering is unsupported.
pub fn hash_set_order(names: &[String]) -> (Vec<String>, bool) {
    let mut table = table_size(names.len());
    let mut inserted = std::collections::HashSet::new();
    let mut hashes = Vec::new();
    let mut counts = std::collections::HashMap::new();
    let mut long = false;
    for name in names {
        if !inserted.insert(name) {
            continue;
        }
        let hash = spread(java_string_hash(name)) as usize;
        hashes.push(hash);
        let count = counts.entry(hash & (table - 1)).or_insert(0);
        *count += 1;
        if *count >= 9 {
            if table < 64 {
                table *= 2;
                counts.clear();
                for hash in &hashes {
                    *counts.entry(hash & (table - 1)).or_insert(0) += 1;
                }
            } else {
                long = true;
            }
        }
    }
    (order_in_table(names, table).0, long)
}

// `hash_set_order` for a given table size (a power of two).
fn order_in_table(names: &[String], table: usize) -> (Vec<String>, bool) {
    let mask = table - 1;
    let mut seen = std::collections::HashSet::new();
    let mut binned: Vec<(usize, &String)> = names
        .iter()
        .filter(|n| seen.insert(n.as_str()))
        .map(|n| (spread(java_string_hash(n)) as usize & mask, n))
        .collect();
    // A stable sort keeps first-insertion order within a bin.
    binned.sort_by_key(|&(bin, _)| bin);
    let long = binned.chunk_by(|a, b| a.0 == b.0).any(|run| run.len() >= 9);
    (binned.into_iter().map(|(_, n)| n.clone()).collect(), long)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_javas_published_values() {
        assert_eq!(java_string_hash(""), 0);
        assert_eq!(java_string_hash("a"), 97);
        assert_eq!(java_string_hash("hello"), 99_162_322);
        // Overflows i32, and wraps as Java does.
        assert_eq!(java_string_hash("polygenelubricants"), i32::MIN);
    }

    #[test]
    fn table_sizes_match_the_constructor() {
        assert_eq!(table_size(12), 16);
        assert_eq!(table_size(24), 32);
        let recorded = include_str!("../../tests/data/p59b_hash21.txt");
        for n in [12, 24] {
            let names: Vec<_> = (0..n).map(|i| format!("grid-{i}")).collect();
            let (order, long) = hash_set_order(&names);
            assert!(!long);
            let indices: Vec<_> = order
                .iter()
                .map(|name| names.iter().position(|x| x == name).unwrap())
                .collect();
            assert!(recorded
                .lines()
                .any(|line| line == format!("n={n} {indices:?}")));
        }
    }

    #[test]
    fn a_collision_keeps_insertion_order_and_a_repeat_its_first_place() {
        // "Aa" and "BB" share a hash.
        assert_eq!(java_string_hash("Aa"), java_string_hash("BB"));
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(hash_set_order(&s(&["Aa", "BB"])).0, s(&["Aa", "BB"]));
        assert_eq!(hash_set_order(&s(&["BB", "Aa"])).0, s(&["BB", "Aa"]));
        assert_eq!(hash_set_order(&s(&["BB", "Aa", "BB"])).0, s(&["BB", "Aa"]));
    }

    #[test]
    fn a_long_bin_is_reported() {
        // Nine strings of one hash, in a table of at least 64 bins, built from Aa and BB.
        let mut names: Vec<String> = Vec::new();
        for i in 0..9u32 {
            let bit = |b: u32| if i >> b & 1 == 1 { "Aa" } else { "BB" };
            names.push(format!("{}{}{}{}", bit(0), bit(1), bit(2), bit(3)));
        }
        names.extend((0..60).map(|i| format!("x{i}")));
        assert!(hash_set_order(&names).1);
    }

    #[test]
    fn small_collision_tables_grow_before_tree_bins() {
        let recorded = include_str!("../../tests/data/p59b_hash21.txt");
        let split: Vec<_> = [
            "c3", "c20", "c31", "c42", "c53", "c64", "c75", "c86", "c97", "c110", "c121", "c132",
        ]
        .map(str::to_owned)
        .into();
        let equal: Vec<_> = (0..16)
            .map(|i| {
                (0..4)
                    .map(|j| if (i >> j) & 1 == 0 { "Aa" } else { "BB" })
                    .collect::<String>()
            })
            .collect();
        for (label, names) in [("split", split), ("equal", equal)] {
            for n in 8..=12 {
                let (order, long) = hash_set_order(&names[..n]);
                assert_eq!(long, label == "equal" && n >= 11, "{label} {n}");
                if !long {
                    let indices: Vec<_> = order
                        .iter()
                        .map(|name| names.iter().position(|x| x == name).unwrap())
                        .collect();
                    assert!(
                        recorded
                            .lines()
                            .any(|line| line == format!("{label}={n} {indices:?}")),
                        "{label} {n}"
                    );
                }
            }
        }
    }

    /// Black-box equivalence with the JDK (P59): 20,000 seeded random lists, the same as
    /// `evidence/P59/scripts/P59JavaHashSetOrder.java`, whose hash Java
    /// 21.0.9+10 prints. The production model uses that runtime's table sizing.
    #[test]
    fn hash_set_order_equals_java_black_box() {
        const PATH: &[u8] = b"/ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_ ";
        let mut state: u64 = 0x5059;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut below = move |n: u64| (next() % n) as usize;
        let mut h21 = 0xcbf2_9ce4_8422_2325u64;
        let eat4 = |h: &mut u64, v: i32| {
            for b in v.to_le_bytes() {
                *h ^= u64::from(b);
                *h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for _ in 0..20_000 {
            let n = 1 + below(300);
            let mut list: Vec<String> = Vec::with_capacity(n);
            for _ in 0..n {
                if !list.is_empty() && below(10) == 0 {
                    let i = below(list.len() as u64);
                    list.push(list[i].clone());
                    continue;
                }
                let len = 1 + below(30);
                let mut s = String::new();
                for _ in 0..len {
                    let cp = match below(4) {
                        0 => 0x20 + below(0x5F) as u32,
                        1 => u32::from(PATH[below(PATH.len() as u64)]),
                        2 => {
                            let v = below(0x10000 - 0x800) as u32;
                            if v >= 0xD800 {
                                v + 0x800
                            } else {
                                v
                            }
                        }
                        _ => 0x10000 + below(0x10_0000) as u32,
                    };
                    s.push(char::from_u32(cp).unwrap());
                }
                list.push(s);
            }
            let position = |x: &String| list.iter().position(|y| y == x).unwrap() as i32;
            let (order, long) = hash_set_order(&list);
            assert!(!long, "the seeded lists hold no long bin");
            for x in &order {
                eat4(&mut h21, position(x));
            }
            eat4(&mut h21, -1);
        }
        assert_eq!(format!("{h21:016x}"), "158089a36cfb2637");
    }
}
