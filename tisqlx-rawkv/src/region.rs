//! Region routing: which region holds a key, and the cache of what PD said.
//!
//! Pure logic, no I/O: the client asks PD on a cache miss and feeds the answer back here.

use std::collections::{BTreeMap, HashMap};

use crate::proto::metapb;

/// A region and the peer requests go to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RegionInfo {
    pub meta: metapb::Region,
    /// The leader PD or TiKV named, if any.
    pub leader: Option<metapb::Peer>,
}

impl RegionInfo {
    pub fn id(&self) -> u64 {
        self.meta.id
    }

    pub fn start_key(&self) -> &[u8] {
        &self.meta.start_key
    }

    /// Empty means the region runs to +∞.
    pub fn end_key(&self) -> &[u8] {
        &self.meta.end_key
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start_key() && (self.end_key().is_empty() || key < self.end_key())
    }

    /// The peer to send to: the known leader, else the first voter (TiKV answers `NotLeader`
    /// with the real leader if it is wrong).
    pub fn target_peer(&self) -> Option<metapb::Peer> {
        self.leader
            .clone()
            .or_else(|| self.meta.peers.iter().find(|p| p.role == 0).cloned())
            .or_else(|| self.meta.peers.first().cloned())
    }

    fn overlaps(&self, other: &RegionInfo) -> bool {
        let starts_before_other_ends = other.end_key().is_empty() || self.start_key() < other.end_key();
        let ends_after_other_starts = self.end_key().is_empty() || self.end_key() > other.start_key();
        starts_before_other_ends && ends_after_other_starts
    }
}

/// The regions the client knows, keyed by start key. Entries never overlap: inserting a
/// region evicts every cached region it overlaps (they are older or newer versions of the
/// same key range, and the newer one wins on the next error anyway).
#[derive(Debug, Default)]
pub(crate) struct RegionCache {
    by_start: BTreeMap<Vec<u8>, RegionInfo>,
    start_of: HashMap<u64, Vec<u8>>,
}

impl RegionCache {
    pub fn locate(&self, key: &[u8]) -> Option<&RegionInfo> {
        let (_, region) =
            self.by_start.range::<[u8], _>((std::ops::Bound::Unbounded, std::ops::Bound::Included(key))).next_back()?;
        region.contains(key).then_some(region)
    }

    pub fn insert(&mut self, region: RegionInfo) {
        let stale: Vec<Vec<u8>> = self
            .by_start
            .iter()
            .filter(|(_, r)| r.overlaps(&region) || r.id() == region.id())
            .map(|(start, _)| start.clone())
            .collect();
        for start in stale {
            if let Some(r) = self.by_start.remove(&start) {
                self.start_of.remove(&r.id());
            }
        }
        self.start_of.insert(region.id(), region.meta.start_key.clone());
        self.by_start.insert(region.meta.start_key.clone(), region);
    }

    pub fn invalidate(&mut self, region_id: u64) {
        if let Some(start) = self.start_of.remove(&region_id) {
            self.by_start.remove(&start);
        }
    }

    /// Records the leader a `NotLeader` error named. False if the region is no longer cached,
    /// or the peer is not one of its peers (then the region itself is stale).
    pub fn update_leader(&mut self, region_id: u64, leader: metapb::Peer) -> bool {
        let Some(start) = self.start_of.get(&region_id) else { return false };
        let Some(region) = self.by_start.get_mut(start) else { return false };
        if !region.meta.peers.iter().any(|p| p.id == leader.id) {
            self.invalidate(region_id);
            return false;
        }
        region.leader = Some(leader);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_start.len()
    }
}

/// The end key of the part of `[.., end)` that lies in a region ending at `region_end`
/// (empty means +∞ for both).
pub(crate) fn clamp_end(region_end: &[u8], end: &[u8]) -> Vec<u8> {
    match (region_end.is_empty(), end.is_empty()) {
        (true, _) => end.to_vec(),
        (false, true) => region_end.to_vec(),
        (false, false) => region_end.min(end).to_vec(),
    }
}

/// True if a range walk that just covered a region ending at `region_end` has reached `end`.
pub(crate) fn walk_done(region_end: &[u8], end: &[u8]) -> bool {
    region_end.is_empty() || (!end.is_empty() && region_end >= end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: u64, start: &[u8], end: &[u8]) -> RegionInfo {
        RegionInfo {
            meta: metapb::Region {
                id,
                start_key: start.to_vec(),
                end_key: end.to_vec(),
                region_epoch: Some(metapb::RegionEpoch { conf_ver: 1, version: 1 }),
                peers: vec![
                    metapb::Peer { id: id * 10 + 1, store_id: 1, role: 1, is_witness: false },
                    metapb::Peer { id: id * 10 + 2, store_id: 2, role: 0, is_witness: false },
                ],
            },
            leader: None,
        }
    }

    #[test]
    fn locate_respects_bounds() {
        let mut c = RegionCache::default();
        c.insert(region(1, b"", b"b"));
        c.insert(region(2, b"b", b"d"));
        c.insert(region(3, b"f", b""));
        assert_eq!(c.locate(b"").map(RegionInfo::id), Some(1));
        assert_eq!(c.locate(b"a\xff").map(RegionInfo::id), Some(1));
        assert_eq!(c.locate(b"b").map(RegionInfo::id), Some(2), "start key is inclusive");
        assert_eq!(c.locate(b"d").map(RegionInfo::id), None, "end key is exclusive; [d, f) is not cached");
        assert_eq!(c.locate(b"e").map(RegionInfo::id), None);
        assert_eq!(c.locate(b"f").map(RegionInfo::id), Some(3));
        assert_eq!(c.locate(b"\xff\xff\xff").map(RegionInfo::id), Some(3), "empty end key is +inf");
    }

    #[test]
    fn insert_evicts_overlapping_regions() {
        let mut c = RegionCache::default();
        c.insert(region(1, b"", b"m"));
        c.insert(region(2, b"m", b""));
        // Region 1 split at "g": the new [g, m) and the shrunk [, g).
        c.insert(region(3, b"g", b"m"));
        assert_eq!(c.locate(b"a"), None, "the old [, m) overlapped and was evicted");
        assert_eq!(c.locate(b"h").map(RegionInfo::id), Some(3));
        assert_eq!(c.locate(b"z").map(RegionInfo::id), Some(2), "adjacent region kept");
        c.insert(region(1, b"", b"g"));
        assert_eq!(c.locate(b"a").map(RegionInfo::id), Some(1));
        assert_eq!(c.len(), 3);
        // A merge covering everything replaces all.
        c.insert(region(9, b"", b""));
        assert_eq!(c.len(), 1);
        assert_eq!(c.locate(b"h").map(RegionInfo::id), Some(9));
    }

    #[test]
    fn same_id_with_new_range_replaces_the_old_entry() {
        let mut c = RegionCache::default();
        c.insert(region(1, b"a", b"c"));
        c.insert(region(1, b"x", b"z"));
        assert_eq!(c.locate(b"b"), None);
        assert_eq!(c.locate(b"y").map(RegionInfo::id), Some(1));
        c.invalidate(1);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn leader_updates() {
        let mut c = RegionCache::default();
        c.insert(region(1, b"", b""));
        assert_eq!(c.locate(b"k").unwrap().target_peer().unwrap().id, 12, "first voter, not the learner");
        let leader = metapb::Peer { id: 11, store_id: 1, role: 0, is_witness: false };
        assert!(c.update_leader(1, leader));
        assert_eq!(c.locate(b"k").unwrap().target_peer().unwrap().id, 11);
        let stranger = metapb::Peer { id: 99, store_id: 7, role: 0, is_witness: false };
        assert!(!c.update_leader(1, stranger), "unknown peer: the region is stale");
        assert_eq!(c.len(), 0);
        assert!(!c.update_leader(1, metapb::Peer::default()));
    }

    #[test]
    fn range_walk_helpers() {
        assert_eq!(clamp_end(b"m", b""), b"m");
        assert_eq!(clamp_end(b"m", b"k"), b"k");
        assert_eq!(clamp_end(b"m", b"z"), b"m");
        assert_eq!(clamp_end(b"", b"z"), b"z");
        assert_eq!(clamp_end(b"", b""), b"");
        assert!(walk_done(b"", b"a"));
        assert!(walk_done(b"m", b"m"));
        assert!(walk_done(b"z", b"m"));
        assert!(!walk_done(b"m", b"z"));
        assert!(!walk_done(b"m", b""));
    }
}
