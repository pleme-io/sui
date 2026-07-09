//! `outcome_chain` — the tick-by-tick attestation surface for the
//! layers-stay-warm Viggy loop.
//!
//! ## Tier-honest (never round up)
//!
//! This is a **BLAKE3 content-addressed hash chain**, NOT an
//! Ed25519-signed OutcomeChain. Each link's id is
//! `BLAKE3(prev_id ‖ canonical_json(record))`, so the chain is
//! **tamper-evident by content-addressing** (any edit to a past link
//! changes every subsequent id) and **append-only** (a link names its
//! predecessor by hash). It is the honest floor of the Viggy fifth beat
//! (continuously-renewed attestation):
//!
//! | Property                          | This chain | A signed OutcomeChain |
//! |-----------------------------------|:----------:|:---------------------:|
//! | append-only / prev-linked         |     ✅     |          ✅           |
//! | tamper-evident (content-addressed)|     ✅     |          ✅           |
//! | **externally verifiable identity**|     ❌     |  ✅ (Ed25519 pubkey)  |
//! | non-repudiation / third-party proof|    ❌     |          ✅           |
//!
//! The missing rung is a signature: an auditor cannot verify *who*
//! produced the chain, only that it is internally consistent. Wiring an
//! `ed25519-dalek` signer (already a workspace dep, the same one
//! `tameshi` uses) over each link's id is the named destination — the
//! `signature: Option<[u8; 64]>` slot is present + always `None` today
//! so promoting to real-signed is an additive change, and
//! [`OutcomeLink::is_signed`] reports the tier so a consumer never
//! mistakes the hash chain for a signed one.

use blake3::Hasher;
use serde::{Deserialize, Serialize};

/// The genesis predecessor id — the all-zero hash every chain's first
/// link names as its `prev`. A real link id is never all-zero (BLAKE3 of
/// any non-trivial input), so genesis is unambiguous.
pub const GENESIS_PREV: [u8; 32] = [0u8; 32];

/// One attested tick record — the typed payload hashed into a chain
/// link. Kept small + `serde`-canonical so the hash is reproducible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// The promessa this tick evaluated (`layers-stay-warm`).
    pub promessa: String,
    /// Monotonic tick counter within this chain (0 = first tick).
    pub tick: u64,
    /// The seen-ratio, in basis points (0..=10_000), so the attested
    /// value is an exact integer — never a lossy float in the hash.
    pub seen_ratio_bps: u16,
    /// The promessa verdict as a stable string (`held` / `breached`).
    pub verdict: String,
    /// The breach reason, if breached (stable string; `None` iff held).
    pub breach: Option<String>,
    /// How many cold watched graphs this tick re-warmed (the Act beat's
    /// applied count).
    pub rewarmed: usize,
}

/// One link in the chain — a record plus its content-addressed id and
/// the predecessor it extends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeLink {
    /// The predecessor link's id (`GENESIS_PREV` for the first link).
    #[serde(with = "hex32")]
    pub prev: [u8; 32],
    /// This link's id — `BLAKE3(prev ‖ canonical_json(record))`.
    #[serde(with = "hex32")]
    pub id: [u8; 32],
    /// The attested tick record.
    pub record: OutcomeRecord,
    /// The Ed25519 signature over `id`. **Always `None` today** — the
    /// named destination that promotes this hash chain to a real-signed
    /// OutcomeChain (tier-honest: absence here IS the honest tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Vec<u8>>,
}

impl OutcomeLink {
    /// Whether this link carries a real signature. `false` for the
    /// shipped hash-chain tier — a consumer uses this to avoid treating
    /// the content-addressed chain as externally verifiable.
    #[must_use]
    pub fn is_signed(&self) -> bool {
        self.signature.is_some()
    }
}

/// Compute the content-addressed id for a record extending `prev`.
/// `BLAKE3(prev ‖ canonical_json(record))`. Canonical because
/// `OutcomeRecord`'s serde shape is fixed field-order; `serde_json`
/// emits struct fields in declaration order, so two identical records
/// hash identically.
fn link_id(prev: &[u8; 32], record: &OutcomeRecord) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(prev);
    // A serialize failure on a plain struct of primitives is
    // unrepresentable; the empty fallback keeps this total without a
    // panic on the (impossible) error path.
    let json = serde_json::to_vec(record).unwrap_or_default();
    hasher.update(&json);
    *hasher.finalize().as_bytes()
}

/// An append-only, content-addressed chain of attested ticks. In-memory
/// (the prewarmer's whole state is in-process); the destination persists
/// links to the DB-backed store, but the *chain algebra* is identical.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutcomeChain {
    links: Vec<OutcomeLink>,
}

impl OutcomeChain {
    /// A fresh, empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of the tip link, or `GENESIS_PREV` for an empty chain.
    #[must_use]
    pub fn head(&self) -> [u8; 32] {
        self.links.last().map_or(GENESIS_PREV, |l| l.id)
    }

    /// The number of attested ticks in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Whether the chain has zero attested ticks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// All links, oldest first.
    #[must_use]
    pub fn links(&self) -> &[OutcomeLink] {
        &self.links
    }

    /// The tip link, if any.
    #[must_use]
    pub fn tip(&self) -> Option<&OutcomeLink> {
        self.links.last()
    }

    /// Append a record, extending the current head. Returns the new
    /// link's id. The `prev` is bound to the current head so the chain
    /// is append-only by construction — a caller cannot fork it.
    pub fn append(&mut self, record: OutcomeRecord) -> [u8; 32] {
        let prev = self.head();
        let id = link_id(&prev, &record);
        self.links.push(OutcomeLink {
            prev,
            id,
            record,
            signature: None,
        });
        id
    }

    /// Verify the whole chain: every link's id is the correct BLAKE3 of
    /// its `prev ‖ record`, and every link's `prev` equals the previous
    /// link's `id` (genesis for the first). Returns the first typed
    /// break, or `Ok(())` if the chain is internally consistent.
    ///
    /// **Tier-honest:** this proves *internal consistency + tamper-
    /// evidence*, NOT *authorship* — there is no signature to check.
    ///
    /// # Errors
    ///
    /// [`ChainBreak::PrevMismatch`] if a link's `prev` does not point at
    /// its actual predecessor; [`ChainBreak::IdMismatch`] if a link's id
    /// is not the recomputed content hash.
    pub fn verify(&self) -> Result<(), ChainBreak> {
        let mut expected_prev = GENESIS_PREV;
        for (idx, link) in self.links.iter().enumerate() {
            if link.prev != expected_prev {
                return Err(ChainBreak::PrevMismatch { index: idx });
            }
            let recomputed = link_id(&link.prev, &link.record);
            if recomputed != link.id {
                return Err(ChainBreak::IdMismatch { index: idx });
            }
            expected_prev = link.id;
        }
        Ok(())
    }
}

/// A typed chain-consistency break, tagged with the link index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChainBreak {
    /// Link `index`'s `prev` does not equal its predecessor's id.
    #[error("chain link #{index}: prev does not point at its predecessor")]
    PrevMismatch { index: usize },
    /// Link `index`'s id is not the recomputed content hash.
    #[error("chain link #{index}: id is not the content hash of its record")]
    IdMismatch { index: usize },
}

/// serde as lowercase hex for the two `[u8; 32]` id fields — keeps a
/// serialized chain human-diffable + JSON-safe (a raw byte array would
/// serialize as a 32-element number list).
mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&data_encoding::HEXLOWER.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let hex = String::deserialize(d)?;
        let raw = data_encoding::HEXLOWER
            .decode(hex.as_bytes())
            .map_err(serde::de::Error::custom)?;
        raw.try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tick: u64, held: bool) -> OutcomeRecord {
        OutcomeRecord {
            promessa: "layers-stay-warm".into(),
            tick,
            seen_ratio_bps: if held { 10_000 } else { 5_000 },
            verdict: if held { "held" } else { "breached" }.into(),
            breach: if held { None } else { Some("seen_ratio_low".into()) },
            rewarmed: usize::from(!held),
        }
    }

    #[test]
    fn empty_chain_head_is_genesis() {
        let chain = OutcomeChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.head(), GENESIS_PREV);
        assert!(chain.verify().is_ok(), "the empty chain is trivially consistent");
    }

    #[test]
    fn appended_link_extends_the_head_and_is_verifiable() {
        let mut chain = OutcomeChain::new();
        let id0 = chain.append(rec(0, true));
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.head(), id0);
        assert_ne!(id0, GENESIS_PREV, "a real link id is never genesis");
        assert_eq!(chain.links()[0].prev, GENESIS_PREV);
        chain.verify().expect("single-link chain verifies");
    }

    #[test]
    fn each_link_prev_points_at_the_prior_link_id() {
        let mut chain = OutcomeChain::new();
        let id0 = chain.append(rec(0, true));
        let id1 = chain.append(rec(1, false));
        let id2 = chain.append(rec(2, true));
        assert_eq!(chain.links()[1].prev, id0);
        assert_eq!(chain.links()[2].prev, id1);
        assert_eq!(chain.head(), id2);
        chain.verify().expect("three-link chain verifies");
    }

    #[test]
    fn tamper_with_a_past_record_is_caught_by_verify() {
        let mut chain = OutcomeChain::new();
        chain.append(rec(0, true));
        chain.append(rec(1, true));
        chain.append(rec(2, true));
        // Mutate a past record without recomputing ids — an attacker
        // rewriting history. verify() must catch the id mismatch.
        chain.links[0].record.seen_ratio_bps = 1;
        let err = chain.verify().unwrap_err();
        assert_eq!(err, ChainBreak::IdMismatch { index: 0 });
    }

    #[test]
    fn a_reordered_prev_pointer_is_caught() {
        let mut chain = OutcomeChain::new();
        chain.append(rec(0, true));
        chain.append(rec(1, true));
        // Break the linkage: point link 1's prev at genesis.
        chain.links[1].prev = GENESIS_PREV;
        // Its id no longer matches prev‖record either, but the prev
        // check fires first with the precise index.
        let err = chain.verify().unwrap_err();
        assert_eq!(err, ChainBreak::PrevMismatch { index: 1 });
    }

    #[test]
    fn identical_records_extending_identical_prev_hash_identically() {
        // Determinism: the content-addressing is reproducible.
        let a = link_id(&GENESIS_PREV, &rec(0, true));
        let b = link_id(&GENESIS_PREV, &rec(0, true));
        assert_eq!(a, b);
        // A different record ⇒ a different id.
        let c = link_id(&GENESIS_PREV, &rec(0, false));
        assert_ne!(a, c);
    }

    #[test]
    fn tier_honest_links_are_unsigned_by_construction() {
        let mut chain = OutcomeChain::new();
        chain.append(rec(0, true));
        // The shipped tier is a hash chain, NOT a signed chain: no link
        // is signed. If a signer ever lands, THIS assertion flips in the
        // same commit (never silently rounded up).
        assert!(chain.links().iter().all(|l| !l.is_signed()));
        assert!(chain.tip().is_some_and(|l| l.signature.is_none()));
    }

    #[test]
    fn chain_round_trips_through_json_with_hex_ids() {
        let mut chain = OutcomeChain::new();
        chain.append(rec(0, true));
        chain.append(rec(1, false));
        let json = serde_json::to_string(&chain).unwrap();
        // ids are hex strings, not number arrays.
        assert!(json.contains("\"id\":\""), "ids serialize as hex strings");
        let parsed: OutcomeChain = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        parsed.verify().expect("a round-tripped chain still verifies");
        assert_eq!(parsed.head(), chain.head());
    }
}
