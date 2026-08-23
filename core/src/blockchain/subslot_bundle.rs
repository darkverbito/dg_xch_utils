// `SubSlotBundle` and `EndOfSubSlotBundle` were field-identical, wire-identical duplicates
// (FullBlock grew up with one name, the gossip protocol with chia's). One struct, two names —
// so the slot state machine, the header validators, and the wire messages all speak the same
// type with no conversion shims.
pub use crate::blockchain::end_of_subslot_bundle::EndOfSubSlotBundle as SubSlotBundle;
