//! Merkle root and inclusion branch over TXIDs (double-SHA256, odd leaf duplicated).
//!
//! Hashes are handled in *internal* (little-endian wire) order for hashing and
//! converted back to display order for callers.

use crate::hash;
use crate::types::Hash32;

/// Hash of two child nodes.
fn sha256d(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    hash::sha256d(&[a, b])
}

/// Compute the merkle root for an ordered list of TXIDs (display order).
pub fn merkle_root(txids: &[Hash32]) -> Option<Hash32> {
    if txids.is_empty() {
        return None;
    }
    let mut level: Vec<[u8; 32]> = txids.iter().map(|h| h.reversed().0).collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = &pair[0];
            let right = if pair.len() == 2 { &pair[1] } else { &pair[0] };
            next.push(sha256d(left, right));
        }
        level = next;
    }
    Some(Hash32(level[0]).reversed())
}

/// Inclusion branch for `pos`, plus the root it produces.
pub fn merkle_branch(txids: &[Hash32], pos: usize) -> Option<(Hash32, Vec<Hash32>)> {
    if txids.is_empty() || pos >= txids.len() {
        return None;
    }
    let mut level: Vec<[u8; 32]> = txids.iter().map(|h| h.reversed().0).collect();
    let mut idx = pos;
    let mut branch = Vec::new();
    while level.len() > 1 {
        let sibling = if idx % 2 == 0 {
            if idx + 1 < level.len() {
                level[idx + 1]
            } else {
                level[idx]
            }
        } else {
            level[idx - 1]
        };
        branch.push(Hash32(sibling).reversed());
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let left = &pair[0];
            let right = if pair.len() == 2 { &pair[1] } else { &pair[0] };
            next.push(sha256d(left, right));
        }
        level = next;
        idx /= 2;
    }
    Some((Hash32(level[0]).reversed(), branch))
}

/// Every level of a block's merkle tree, kept so that many inclusion branches
/// of the same block cost one tree instead of one tree each. A wallet asks
/// for the proof of every transaction of its history, often hundreds from the
/// same block. Costs about as much memory as the txids themselves again.
#[derive(Debug, Clone)]
pub struct MerkleTree {
    /// `levels[0]` are the leaves (internal byte order), the last is the root.
    levels: Vec<Vec<[u8; 32]>>,
}

impl MerkleTree {
    pub fn new(txids: &[Hash32]) -> Option<Self> {
        if txids.is_empty() {
            return None;
        }
        let mut levels = vec![txids.iter().map(|h| h.reversed().0).collect::<Vec<_>>()];
        while levels[levels.len() - 1].len() > 1 {
            let level = &levels[levels.len() - 1];
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            for pair in level.chunks(2) {
                let left = &pair[0];
                let right = if pair.len() == 2 { &pair[1] } else { &pair[0] };
                next.push(sha256d(left, right));
            }
            levels.push(next);
        }
        Some(Self { levels })
    }

    pub fn root(&self) -> Hash32 {
        Hash32(self.levels[self.levels.len() - 1][0]).reversed()
    }

    /// Inclusion branch for the leaf at `pos`, as `merkle_branch` returns it.
    pub fn branch(&self, pos: usize) -> Option<Vec<Hash32>> {
        if pos >= self.levels[0].len() {
            return None;
        }
        let mut idx = pos;
        let mut branch = Vec::with_capacity(self.levels.len() - 1);
        for level in &self.levels[..self.levels.len() - 1] {
            let sibling = if idx % 2 == 0 {
                level.get(idx + 1).unwrap_or(&level[idx])
            } else {
                &level[idx - 1]
            };
            branch.push(Hash32(*sibling).reversed());
            idx /= 2;
        }
        Some(branch)
    }
}

/// Root reached from `leaf` at `pos` through `branch` (all in display order).
/// The inverse of `merkle_branch`: a cached transaction is authenticated by
/// comparing this with the indexed header's merkle root.
pub fn merkle_root_from_branch(leaf: Hash32, branch: &[Hash32], pos: usize) -> Hash32 {
    let mut cur = leaf.reversed().0;
    let mut idx = pos;
    for sib in branch {
        let s = sib.reversed().0;
        cur = if idx % 2 == 0 {
            sha256d(&cur, &s)
        } else {
            sha256d(&s, &cur)
        };
        idx /= 2;
    }
    Hash32(cur).reversed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash32 {
        let mut a = [0u8; 32];
        a[0] = n;
        Hash32(a)
    }

    /// The kept tree gives the same root and the same branch as the
    /// one-shot functions, for every position of every small block shape
    /// (single transaction, odd and even levels).
    #[test]
    fn the_kept_tree_matches_the_one_shot_branch() {
        for n in 1..=33u8 {
            let txids: Vec<Hash32> = (1..=n).map(h).collect();
            let tree = MerkleTree::new(&txids).unwrap();
            assert_eq!(tree.root(), merkle_root(&txids).unwrap(), "n={n}");
            for pos in 0..txids.len() {
                let (root, branch) = merkle_branch(&txids, pos).unwrap();
                assert_eq!(tree.branch(pos).unwrap(), branch, "n={n} pos={pos}");
                assert_eq!(merkle_root_from_branch(txids[pos], &branch, pos), root);
            }
            assert!(tree.branch(txids.len()).is_none());
        }
        assert!(MerkleTree::new(&[]).is_none());
    }

    #[test]
    fn single_tx_root_is_txid() {
        assert_eq!(merkle_root(&[h(1)]).unwrap(), h(1));
    }

    #[test]
    fn two_tx_root_matches_manual_double_sha() {
        let a = h(1);
        let b = h(2);
        let expect = Hash32(sha256d(&a.reversed().0, &b.reversed().0)).reversed();
        assert_eq!(merkle_root(&[a, b]).unwrap(), expect);
    }

    #[test]
    fn odd_leaf_is_duplicated() {
        let a = h(1);
        let b = h(2);
        let c = h(3);
        let lvl1_ab = sha256d(&a.reversed().0, &b.reversed().0);
        let lvl1_cc = sha256d(&c.reversed().0, &c.reversed().0);
        let root = Hash32(sha256d(&lvl1_ab, &lvl1_cc)).reversed();
        assert_eq!(merkle_root(&[a, b, c]).unwrap(), root);
    }

    #[test]
    fn branch_reconstructs_root() {
        for n in 1..=9usize {
            let txids: Vec<Hash32> = (0..n as u8).map(h).collect();
            let root = merkle_root(&txids).unwrap();
            for pos in 0..n {
                let (computed, branch) = merkle_branch(&txids, pos).unwrap();
                assert_eq!(computed, root, "n={n} pos={pos}");
                // Rebuild from leaf.
                let mut cur = txids[pos].reversed().0;
                let mut idx = pos;
                for sib in branch {
                    let s = sib.reversed().0;
                    cur = if idx % 2 == 0 {
                        sha256d(&cur, &s)
                    } else {
                        sha256d(&s, &cur)
                    };
                    idx /= 2;
                }
                assert_eq!(Hash32(cur).reversed(), root);
                assert_eq!(
                    merkle_root_from_branch(
                        txids[pos],
                        &merkle_branch(&txids, pos).unwrap().1,
                        pos
                    ),
                    root
                );
            }
        }
    }
}
