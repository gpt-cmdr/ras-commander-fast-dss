//! Memoized hash-chain positions for catalog validation.
//!
//! Both catalog walks check that every live entry's bin lies on the overflow chain that
//! starts at its hash-table slot: DSS7 `dss7/walk.rs::check_hashes` (rule from
//! `heclib_c/src/Internal/zcheckInternal.c:zcheckInternal()`, which follows
//! `kbinSize - 1` overflow links from `fileHeader[kaddTableHash + tableHash]`), DSS6
//! `dss6/walk.rs::verify_chains` (`heclib_f/src/zcheck6.f` lines 211-322). The original
//! code walked the chain again for every entry, so cost grew with entries times chain
//! prefix; files with many entries in long or orphaned chains paid most.
//!
//! [`ChainHops`] walks each distinct chain start once, records the first hop at which
//! every address appears, and answers later queries from that record. The answer is the
//! one the per-entry walk gives:
//!
//! - The per-entry walk visits `a0, next(a0), ...` while the address is non-zero and at
//!   most `limit` addresses have been visited, and returns the hop of the first match.
//! - `next` is a function of the address alone, so once an address repeats every later
//!   address repeats too. Stopping at the first repeat therefore loses no first
//!   occurrence, and hops stay below `limit` because the record stops there as well.
//!
//! Memory is bounded: once `budget` addresses have been recorded over all chains (at
//! most one chain beyond it), queries for chain starts not yet recorded fall back to the
//! per-entry walk, so malformed files whose many slots share one long chain cost no more
//! than before.

use std::collections::HashMap;

/// Chains longer than this get a hash index for lookups; shorter ones are scanned.
const INDEX_FROM: usize = 32;

/// Overflow successor of `a` as both walks read it: 0 when `a` is not a walked bin.
#[inline]
fn next(overflow: &HashMap<i64, i64>, a: i64) -> i64 {
    overflow.get(&a).copied().unwrap_or(0)
}

/// The per-entry walk: hop of the first visit of `target` along the chain from `start`,
/// visiting at most `limit` addresses. Kept as the fallback and as the reference.
pub(crate) fn walk_once(
    overflow: &HashMap<i64, i64>,
    start: i64,
    target: i64,
    limit: usize,
    steps: &mut u64,
) -> Option<u32> {
    let mut a = start;
    let mut hop = 0usize;
    while a != 0 && hop < limit {
        if a == target {
            return Some(hop as u32);
        }
        a = next(overflow, a);
        hop += 1;
        *steps += 1;
    }
    None
}

/// First-visit order of one chain, with an index when long.
struct Chain {
    order: Vec<i64>,
    index: Option<HashMap<i64, u32>>,
}

impl Chain {
    fn hop(&self, target: i64) -> Option<u32> {
        match &self.index {
            Some(ix) => ix.get(&target).copied(),
            None => self
                .order
                .iter()
                .position(|&a| a == target)
                .map(|p| p as u32),
        }
    }
}

/// Memoized chain positions; see the module documentation.
pub(crate) struct ChainHops<'a> {
    overflow: &'a HashMap<i64, i64>,
    limit: usize,
    chains: HashMap<i64, Chain>,
    budget: usize,
    /// Overflow links followed (recording plus fallback walks).
    pub(crate) steps: u64,
    /// Chain starts recorded, and queries answered by the fallback walk.
    pub(crate) recorded: u64,
    pub(crate) fallbacks: u64,
}

impl<'a> ChainHops<'a> {
    /// `limit` is the most addresses the per-entry walk visits; `budget` caps the
    /// addresses kept over all chains.
    pub(crate) fn new(overflow: &'a HashMap<i64, i64>, limit: usize, budget: usize) -> Self {
        ChainHops {
            overflow,
            limit,
            chains: HashMap::new(),
            budget,
            steps: 0,
            recorded: 0,
            fallbacks: 0,
        }
    }

    /// Budget for a walk over `bins` bins and `slots` table slots: a well-formed file
    /// has every bin on exactly one chain, so this keeps all of them.
    pub(crate) fn default_budget(bins: usize, slots: usize) -> usize {
        bins.saturating_add(slots)
            .saturating_mul(2)
            .saturating_add(1024)
    }

    /// Hop of the first visit of `target` along the chain from `start`, exactly as
    /// [`walk_once`] returns it.
    pub(crate) fn hop(&mut self, start: i64, target: i64) -> Option<u32> {
        if start == 0 || self.limit == 0 {
            return None;
        }
        if let Some(c) = self.chains.get(&start) {
            return c.hop(target);
        }
        if self.budget == 0 {
            self.fallbacks += 1;
            return walk_once(self.overflow, start, target, self.limit, &mut self.steps);
        }
        let chain = self.record(start);
        let hop = chain.hop(target);
        self.chains.insert(start, chain);
        hop
    }

    /// Record the chain from `start` up to its first repeat or `limit` addresses. The
    /// budget is checked before a record starts, so it can be exceeded by at most one
    /// chain (at most `limit` addresses, which the per-entry walk visits anyway).
    fn record(&mut self, start: i64) -> Chain {
        self.recorded += 1;
        let mut order: Vec<i64> = Vec::new();
        let mut index: Option<HashMap<i64, u32>> = None;
        let mut a = start;
        while a != 0 && order.len() < self.limit {
            let seen = match &index {
                Some(ix) => ix.contains_key(&a),
                None => order.contains(&a),
            };
            if seen {
                // Every later address repeats an earlier one.
                break;
            }
            if let Some(ix) = &mut index {
                ix.insert(a, order.len() as u32);
            }
            order.push(a);
            self.budget = self.budget.saturating_sub(1);
            if index.is_none() && order.len() > INDEX_FROM {
                index = Some(
                    order
                        .iter()
                        .enumerate()
                        .map(|(i, &x)| (x, i as u32))
                        .collect(),
                );
            }
            a = next(self.overflow, a);
            self.steps += 1;
        }
        Chain { order, index }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(links: &[(i64, i64)]) -> HashMap<i64, i64> {
        links.iter().copied().collect()
    }

    /// Every (start, target) pair over the addresses in `ov` (plus 0, a dangling and an
    /// absent address) answers as the per-entry walk does, for several limits/budgets.
    fn check_all(ov: &HashMap<i64, i64>) {
        let mut addrs: Vec<i64> = ov.keys().copied().chain(ov.values().copied()).collect();
        addrs.extend([0, 999_999, -5]);
        addrs.sort_unstable();
        addrs.dedup();
        for limit in [0usize, 1, 2, 3, ov.len() + 1, ov.len() + 2, 1000] {
            for budget in [0usize, 1, 3, 7, usize::MAX] {
                let mut memo = ChainHops::new(ov, limit, budget);
                for &s in &addrs {
                    for &t in &addrs {
                        let mut st = 0;
                        let want = walk_once(ov, s, t, limit, &mut st);
                        assert_eq!(
                            memo.hop(s, t),
                            want,
                            "start {s} target {t} limit {limit} budget {budget}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn matches_walk_on_malformed_chains() {
        // Plain chain, self loop, two-cycle reached from a tail, two chains merging,
        // dangling link (to a bin not walked), zero-terminated and a long chain.
        let mut long: Vec<(i64, i64)> = (1000..1100).map(|a| (a, a + 1)).collect();
        long.push((1100, 1050)); // loop back into the middle
        let cases: Vec<HashMap<i64, i64>> = vec![
            map(&[(10, 20), (20, 30), (30, 0)]),
            map(&[(10, 10)]),
            map(&[(1, 2), (2, 3), (3, 4), (4, 3)]),
            map(&[(1, 5), (2, 5), (5, 6), (6, 0), (7, 6)]),
            map(&[(1, 2), (2, 77), (3, 0)]),
            map(&[]),
            long.iter().copied().collect(),
        ];
        for ov in &cases {
            check_all(ov);
        }
    }

    #[test]
    fn work_is_one_walk_per_chain_start() {
        // 200 starts into one 500-bin chain (merged chains): the per-entry walk costs
        // about entries * chain length; the memo walks each start once.
        let mut ov: HashMap<i64, i64> = (1..500).map(|a| (a, a + 1)).collect();
        ov.insert(500, 0);
        let limit = ov.len() + 1;
        let mut memo = ChainHops::new(&ov, limit, ChainHops::default_budget(500, 1));
        let mut reference = 0u64;
        for q in 0..20_000i64 {
            let start = 1 + (q % 2);
            let target = 1 + (q * 7919) % 600; // some absent
            let mut st = 0;
            assert_eq!(
                memo.hop(start, target),
                walk_once(&ov, start, target, limit, &mut st)
            );
            reference += st;
        }
        assert_eq!(memo.recorded, 2);
        assert_eq!(memo.fallbacks, 0);
        assert!(memo.steps <= 1000, "{}", memo.steps);
        assert!(reference > 1_000_000, "{reference}");
    }

    #[test]
    fn memory_is_bounded_on_shared_chains() {
        // 1000 distinct starts that all merge into one 2000-bin chain: a full record per
        // start would keep 2 million addresses; the budget keeps the total bounded and
        // later starts fall back to the per-entry walk.
        let mut ov: HashMap<i64, i64> = (1..2000).map(|a| (a, a + 1)).collect();
        for s in 0..1000i64 {
            ov.insert(10_000 + s, 1);
        }
        let limit = ov.len() + 1;
        let budget = 5000;
        let mut memo = ChainHops::new(&ov, limit, budget);
        for s in 0..1000i64 {
            let mut st = 0;
            assert_eq!(
                memo.hop(10_000 + s, 1999),
                walk_once(&ov, 10_000 + s, 1999, limit, &mut st)
            );
        }
        let kept: usize = memo.chains.values().map(|c| c.order.len()).sum();
        assert!(kept <= budget + limit, "{kept}");
        assert!(memo.fallbacks > 0);
    }
}
