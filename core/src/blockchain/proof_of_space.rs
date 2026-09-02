use crate::blockchain::sized_bytes::{Bytes32, Bytes48};
use crate::clvm::sexp::SExp;
use crate::consensus::constants::ConsensusConstants;
use crate::formatting::prep_hex_str;
use crate::traits::SizedBytes;
#[cfg(feature = "bls")]
use crate::utils::hash_256;
#[cfg(feature = "bls")]
use blst::min_pk::{AggregatePublicKey, PublicKey, SecretKey};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use hex::{decode, encode};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::cmp::max;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
#[cfg(feature = "bls")]
use std::io::ErrorKind;
use std::io::{Cursor, Error};

pub const NUMBER_ZERO_BITS_PLOT_FILTER: i32 = 9;

#[derive(Clone, PartialEq, Eq)]
pub struct ProofBytes(Vec<u8>);

impl IntoIterator for ProofBytes {
    type Item = u8;
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl Display for ProofBytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", encode(&self.0))
    }
}
impl Debug for ProofBytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", encode(&self.0))
    }
}

impl ChiaSerialize for ProofBytes {
    fn to_bytes(&self, version: ChiaProtocolVersion) -> Result<Vec<u8>, Error>
    where
        Self: Sized,
    {
        ChiaSerialize::to_bytes(&self.0, version)
    }

    fn from_bytes(bytes: &mut Cursor<&[u8]>, version: ChiaProtocolVersion) -> Result<Self, Error>
    where
        Self: Sized,
    {
        Ok(Self(ChiaSerialize::from_bytes(bytes, version)?))
    }
}

impl Serialize for ProofBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", encode(&self.0)))
    }
}

struct ProofBytesVisitor;

impl Visitor<'_> for ProofBytesVisitor {
    type Value = ProofBytes;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Expecting a hex String")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ProofBytes(
            decode(prep_hex_str(value)).map_err(|e| serde::de::Error::custom(e.to_string()))?,
        ))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(ProofBytes(
            decode(prep_hex_str(&value)).map_err(|e| serde::de::Error::custom(e.to_string()))?,
        ))
    }
}

impl<'a> Deserialize<'a> for ProofBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        match deserializer.deserialize_string(ProofBytesVisitor) {
            Ok(hex) => Ok(hex),
            Err(er) => Err(er),
        }
    }
}

impl AsRef<[u8]> for ProofBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<Vec<u8>> for ProofBytes {
    fn from(bytes: Vec<u8>) -> ProofBytes {
        ProofBytes(bytes)
    }
}

impl From<&ProofBytes> for SExp<'static> {
    fn from(bytes: &ProofBytes) -> SExp<'static> {
        SExp::from(bytes.0.clone())
    }
}

// Serialization is hand written: the wire format packs the proof version into bit 1 of the
// pool_contract_puzzle_hash Option prefix, which a per-field derive cannot express. A v1 proof
// writes prefix 0b00/0b01 and carries size; a v2 proof writes 0b10/0b11 and carries plot_index,
// meta_group and strength in its place.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct ProofOfSpace {
    pub challenge: Bytes32,
    pub pool_public_key: Option<Bytes48>,
    pub pool_contract_puzzle_hash: Option<Bytes32>,
    pub plot_public_key: Bytes48,
    /// 0 for a v1 proof, 1 for a v2 proof. The four v2 fields default on deserialize so JSON
    /// written before v2 proofs existed still reads as a v1 proof.
    #[serde(default)]
    pub version: u8,
    /// v2 only; zero on v1 proofs.
    #[serde(default)]
    pub plot_index: u16,
    /// v2 only; zero on v1 proofs.
    #[serde(default)]
    pub meta_group: u8,
    /// v2 only; zero on v1 proofs.
    #[serde(default)]
    pub strength: u8,
    /// v1 only; zero on v2 proofs, whose plot size is a network constant.
    pub size: u8,
    pub proof: ProofBytes,
}
impl ProofOfSpace {
    #[must_use]
    pub fn v1(
        challenge: Bytes32,
        pool_public_key: Option<Bytes48>,
        pool_contract_puzzle_hash: Option<Bytes32>,
        plot_public_key: Bytes48,
        size: u8,
        proof: ProofBytes,
    ) -> Self {
        Self {
            challenge,
            pool_public_key,
            pool_contract_puzzle_hash,
            plot_public_key,
            version: 0,
            plot_index: 0,
            meta_group: 0,
            strength: 0,
            size,
            proof,
        }
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn v2(
        challenge: Bytes32,
        pool_public_key: Option<Bytes48>,
        pool_contract_puzzle_hash: Option<Bytes32>,
        plot_public_key: Bytes48,
        plot_index: u16,
        meta_group: u8,
        strength: u8,
        proof: ProofBytes,
    ) -> Self {
        Self {
            challenge,
            pool_public_key,
            pool_contract_puzzle_hash,
            plot_public_key,
            version: 1,
            plot_index,
            meta_group,
            strength,
            size: 0,
            proof,
        }
    }

    #[must_use]
    pub fn get_plot_id(&self) -> Option<Bytes32> {
        if let (Some(_), Some(_)) = (&self.pool_public_key, &self.pool_contract_puzzle_hash) {
            //Invalid, Both cant be Some
            None
        } else if let (None, None) = (&self.pool_public_key, &self.pool_contract_puzzle_hash) {
            //Invalid, Both cant be None
            None
        } else if self.version == 1 {
            Some(calculate_plot_id_v2(
                self.strength,
                self.plot_public_key,
                self.pool_public_key,
                self.pool_contract_puzzle_hash,
                self.plot_index,
                self.meta_group,
            ))
        } else if let Some(contract) = self.pool_contract_puzzle_hash {
            Some(calculate_plot_id_puzzle_hash(
                contract,
                self.plot_public_key,
            ))
        } else {
            self.pool_public_key
                .map(|pub_key| calculate_plot_id_public_key(pub_key, self.plot_public_key))
        }
    }
}

impl ChiaSerialize for ProofOfSpace {
    fn to_bytes(&self, version: ChiaProtocolVersion) -> Result<Vec<u8>, Error> {
        let mut bytes = ChiaSerialize::to_bytes(&self.challenge, version)?;
        bytes.extend(ChiaSerialize::to_bytes(&self.pool_public_key, version)?);
        match self.version {
            0 => {
                bytes.extend(ChiaSerialize::to_bytes(
                    &self.pool_contract_puzzle_hash,
                    version,
                )?);
                bytes.extend(ChiaSerialize::to_bytes(&self.plot_public_key, version)?);
                bytes.extend(ChiaSerialize::to_bytes(&self.size, version)?);
            }
            1 => {
                if let Some(contract) = &self.pool_contract_puzzle_hash {
                    bytes.push(0b11);
                    bytes.extend(ChiaSerialize::to_bytes(contract, version)?);
                } else {
                    bytes.push(0b10);
                }
                bytes.extend(ChiaSerialize::to_bytes(&self.plot_public_key, version)?);
                bytes.extend(ChiaSerialize::to_bytes(&self.plot_index, version)?);
                bytes.extend(ChiaSerialize::to_bytes(&self.meta_group, version)?);
                bytes.extend(ChiaSerialize::to_bytes(&self.strength, version)?);
            }
            other => {
                return Err(Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown proof of space version: {other}"),
                ));
            }
        }
        bytes.extend(ChiaSerialize::to_bytes(&self.proof, version)?);
        Ok(bytes)
    }

    fn from_bytes(bytes: &mut Cursor<&[u8]>, version: ChiaProtocolVersion) -> Result<Self, Error> {
        let challenge = ChiaSerialize::from_bytes(bytes, version)?;
        let pool_public_key: Option<Bytes48> = ChiaSerialize::from_bytes(bytes, version)?;
        let prefix: u8 = ChiaSerialize::from_bytes(bytes, version)?;
        // Only the Option flag (bit 0) and the proof version (bit 1) carry meaning; anything above
        // is malformed, exactly as a plain Option prefix above 1 always was.
        if prefix > 0b11 {
            return Err(Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid proof of space prefix: {prefix}"),
            ));
        }
        let proof_version = prefix >> 1;
        let pool_contract_puzzle_hash: Option<Bytes32> = if prefix & 1 != 0 {
            Some(ChiaSerialize::from_bytes(bytes, version)?)
        } else {
            None
        };
        let plot_public_key = ChiaSerialize::from_bytes(bytes, version)?;
        if proof_version == 0 {
            let size = ChiaSerialize::from_bytes(bytes, version)?;
            let proof = ChiaSerialize::from_bytes(bytes, version)?;
            Ok(Self::v1(
                challenge,
                pool_public_key,
                pool_contract_puzzle_hash,
                plot_public_key,
                size,
                proof,
            ))
        } else {
            let plot_index = ChiaSerialize::from_bytes(bytes, version)?;
            let meta_group = ChiaSerialize::from_bytes(bytes, version)?;
            let strength = ChiaSerialize::from_bytes(bytes, version)?;
            let proof = ChiaSerialize::from_bytes(bytes, version)?;
            if pool_public_key.is_some() == pool_contract_puzzle_hash.is_some() {
                return Err(Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a v2 proof needs exactly one of pool_public_key and pool_contract_puzzle_hash",
                ));
            }
            Ok(Self::v2(
                challenge,
                pool_public_key,
                pool_contract_puzzle_hash,
                plot_public_key,
                plot_index,
                meta_group,
                strength,
                proof,
            ))
        }
    }
}

impl From<&ProofOfSpace> for SExp<'static> {
    fn from(val: &ProofOfSpace) -> SExp<'static> {
        (&[
            SExp::from(&val.challenge),
            SExp::from(&val.pool_public_key),
            SExp::from(&val.pool_contract_puzzle_hash),
            SExp::from(&val.plot_public_key),
            SExp::from(&val.version),
            SExp::from(&val.plot_index),
            SExp::from(&val.meta_group),
            SExp::from(&val.strength),
            SExp::from(&val.size),
            SExp::from(&val.proof),
        ])
            .into()
    }
}
impl From<ProofOfSpace> for SExp<'static> {
    fn from(val: ProofOfSpace) -> SExp<'static> {
        (&val).into()
    }
}

/// `plot_group_id = sha256(strength || plot_pk || (pool_pk | contract_ph))`. Every plot in a group
/// shares this; the per plot id folds the index and meta group on top.
#[must_use]
pub fn calculate_plot_group_id_v2(
    strength: u8,
    plot_public_key: Bytes48,
    pool_public_key: Option<Bytes48>,
    pool_contract_puzzle_hash: Option<Bytes32>,
) -> Bytes32 {
    let mut hasher: Sha256 = Sha256::new();
    hasher.update([strength]);
    hasher.update(plot_public_key);
    if let Some(pool) = pool_public_key {
        hasher.update(pool);
    } else if let Some(contract) = pool_contract_puzzle_hash {
        hasher.update(contract);
    }
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

/// `plot_id = sha256(plot_group_id || plot_index || meta_group)`, the index big endian.
#[must_use]
pub fn calculate_plot_id_v2(
    strength: u8,
    plot_public_key: Bytes48,
    pool_public_key: Option<Bytes48>,
    pool_contract_puzzle_hash: Option<Bytes32>,
    plot_index: u16,
    meta_group: u8,
) -> Bytes32 {
    let group = calculate_plot_group_id_v2(
        strength,
        plot_public_key,
        pool_public_key,
        pool_contract_puzzle_hash,
    );
    let mut hasher: Sha256 = Sha256::new();
    hasher.update(group);
    hasher.update(plot_index.to_be_bytes());
    hasher.update([meta_group]);
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

#[must_use]
pub fn calculate_plot_id_public_key(pool_public_key: Bytes48, plot_public_key: Bytes48) -> Bytes32 {
    let mut to_hash: Vec<u8> = Vec::new();
    to_hash.extend(pool_public_key);
    to_hash.extend(plot_public_key);
    let mut hasher: Sha256 = Sha256::new();
    hasher.update(to_hash);
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

#[must_use]
pub fn calculate_plot_id_puzzle_hash(
    pool_contract_puzzle_hash: Bytes32,
    plot_public_key: Bytes48,
) -> Bytes32 {
    let mut to_hash: Vec<u8> = Vec::new();
    to_hash.extend(pool_contract_puzzle_hash);
    to_hash.extend(plot_public_key);
    let mut hasher: Sha256 = Sha256::new();
    hasher.update(to_hash);
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

/// The number of v1 phase-out epochs: always a power of two minus one, so it doubles as a bit mask
/// over the phase-out hash.
#[must_use]
pub fn num_phase_out_epochs(constants: &ConsensusConstants) -> u32 {
    (1u32 << constants.plot_v1_phase_out_epoch_bits) - 1
}

/// The height at which v1 proofs stop being valid: a block whose previous transaction block is at
/// or above this may not carry one.
#[must_use]
pub fn v1_cut_off_height(constants: &ConsensusConstants) -> u64 {
    u64::from(constants.hard_fork2_height)
        + u64::from(num_phase_out_epochs(constants)) * u64::from(constants.epoch_blocks)
}

/// Whether a v1 proof has been phased out.
///
/// Before hard fork 2 nothing is phased out. After it, each proof is retired at a randomly assigned
/// epoch: the phase-out byte of `hash(proof || tag)` is compared against a counter that ticks down
/// to the cut-off height, so the surviving fraction of v1 plots shrinks epoch by epoch until none
/// remain.
#[must_use]
pub fn is_v1_phased_out(
    proof: &[u8],
    prev_transaction_block_height: u32,
    constants: &ConsensusConstants,
) -> bool {
    if prev_transaction_block_height < constants.hard_fork2_height {
        return false;
    }
    let mask = num_phase_out_epochs(constants);
    debug_assert!(mask < 256, "phase-out mask must fit one byte");

    let cut_off = v1_cut_off_height(constants) as i64;
    let epoch_counter =
        (cut_off - i64::from(prev_transaction_block_height)) / i64::from(constants.epoch_blocks);
    if epoch_counter < 0 {
        return true;
    }

    let mut hasher = Sha256::new();
    hasher.update(proof);
    hasher.update(b"chia proof-of-space v1 phase-out");
    let mut digest = [0u8; 32];
    hasher.finalize_into((&mut digest).into());
    let proof_value = i64::from(digest[0] & mask as u8);
    proof_value >= epoch_counter
}

/// The v2 plot filter, on its own schedule: v2 plots start at five zero bits and relax by one at
/// each adjustment height, against v1's nine.
#[allow(clippy::cast_possible_wrap)]
#[must_use]
pub fn calculate_prefix_bits_v2(constants: &ConsensusConstants, height: u32) -> i8 {
    let mut prefix_bits = constants.number_zero_bits_plot_filter_v2 as i8;
    if height >= constants.plot_filter_v2_third_adjustment_height {
        prefix_bits -= 3;
    } else if height >= constants.plot_filter_v2_second_adjustment_height {
        prefix_bits -= 2;
    } else if height >= constants.plot_filter_v2_first_adjustment_height {
        prefix_bits -= 1;
    }
    max(0, prefix_bits)
}

#[allow(clippy::cast_possible_wrap)]
#[must_use]
pub fn calculate_prefix_bits(constants: &ConsensusConstants, height: u32) -> i8 {
    let mut prefix_bits = constants.number_zero_bits_plot_filter as i8;
    if height >= constants.plot_filter_32_height {
        prefix_bits -= 4;
    } else if height >= constants.plot_filter_64_height {
        prefix_bits -= 3;
    } else if height >= constants.plot_filter_128_height {
        prefix_bits -= 2;
    } else if height >= constants.hard_fork_height {
        prefix_bits -= 1;
    }
    max(0, prefix_bits)
}

#[allow(clippy::cast_sign_loss)]
#[must_use]
pub fn passes_plot_filter(
    prefix_bits: i8,
    plot_id: Bytes32,
    challenge_hash: Bytes32,
    signage_point: Bytes32,
) -> bool {
    if prefix_bits == 0 {
        true
    } else {
        let mut filter = [false; 256];
        let mut index = 0;
        for b in calculate_plot_filter_input(plot_id, challenge_hash, signage_point).bytes() {
            for i in (0..=7).rev() {
                filter[index] = ((b >> i) & 1) == 1;
                index += 1;
            }
        }
        for is_one in filter.iter().take(prefix_bits as usize) {
            if *is_one {
                return false;
            }
        }
        true
    }
}

#[must_use]
pub fn calculate_plot_filter_input(
    plot_id: Bytes32,
    challenge_hash: Bytes32,
    signage_point: Bytes32,
) -> Bytes32 {
    let mut hasher: Sha256 = Sha256::new();
    hasher.update(plot_id);
    hasher.update(challenge_hash);
    hasher.update(signage_point);
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

#[must_use]
pub fn calculate_pos_challenge(
    plot_id: Bytes32,
    challenge_hash: Bytes32,
    signage_point: Bytes32,
) -> Bytes32 {
    let mut hasher: Sha256 = Sha256::new();
    hasher.update(calculate_plot_filter_input(
        plot_id,
        challenge_hash,
        signage_point,
    ));
    let mut buf = [0u8; 32];
    hasher.finalize_into((&mut buf).into());
    buf.into()
}

#[cfg(feature = "bls")]
pub fn generate_taproot_sk(
    local_pk: &PublicKey,
    farmer_pk: &PublicKey,
) -> Result<SecretKey, Error> {
    let mut taproot_message = vec![];
    let mut agg = AggregatePublicKey::from_public_key(local_pk);
    agg.add_public_key(farmer_pk, false)
        .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("{e:?}")))?;
    taproot_message.extend(agg.to_public_key().to_bytes());
    taproot_message.extend(local_pk.to_bytes());
    taproot_message.extend(farmer_pk.to_bytes());
    let taproot_hash = hash_256(&taproot_message);
    SecretKey::key_gen_v3(&taproot_hash, &[])
        .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("{e:?}")))
}

#[cfg(feature = "bls")]
pub fn generate_plot_public_key(
    local_pk: &PublicKey,
    farmer_pk: &PublicKey,
    include_taproot: bool,
) -> Result<PublicKey, Error> {
    let mut agg = AggregatePublicKey::from_public_key(local_pk);
    if include_taproot {
        let taproot_sk = generate_taproot_sk(local_pk, farmer_pk)?;
        agg.add_public_key(farmer_pk, false)
            .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("{e:?}")))?;
        agg.add_public_key(&taproot_sk.sk_to_pk(), false)
            .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("{e:?}")))?;
        Ok(agg.to_public_key())
    } else {
        agg.add_public_key(farmer_pk, false)
            .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("{e:?}")))?;
        Ok(agg.to_public_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixture keys behind the plot id vectors below.
    fn plot_pk() -> Bytes48 {
        Bytes48::try_from(
            "96b35c22adf93068c9536e016e88251ad715a591d8deabb60917d9c495f45a220ca56b906793c27778d5f7f71fb50b94",
        )
        .expect("hex")
    }
    fn pool_pk() -> Bytes48 {
        Bytes48::try_from(
            "ac6e995e0f9c307853fa5c79e571de5ec2f2d45e5c2641c0847fef8041916e4d07d5a9200d5aa92ceac3b1bf41ce93b2",
        )
        .expect("hex")
    }

    fn make_pos(version: u8, has_pool_pk: bool, has_contract: bool, size: u8) -> ProofOfSpace {
        ProofOfSpace {
            challenge: Bytes32::default(),
            pool_public_key: has_pool_pk.then(pool_pk),
            pool_contract_puzzle_hash: has_contract.then(Bytes32::default),
            plot_public_key: plot_pk(),
            version,
            plot_index: 0,
            meta_group: 0,
            strength: 0,
            size,
            proof: ProofBytes::from(vec![0x80]),
        }
    }

    fn round_trip(pos: &ProofOfSpace) -> ProofOfSpace {
        let bytes = pos
            .to_bytes(ChiaProtocolVersion::default())
            .expect("serializes");
        let parsed =
            ProofOfSpace::from_bytes_exact(&bytes, ChiaProtocolVersion::default()).expect("parses");
        assert_eq!(
            parsed
                .to_bytes(ChiaProtocolVersion::default())
                .expect("re-serializes"),
            bytes,
            "re-serialization is not byte stable"
        );
        parsed
    }

    #[test]
    fn a_v1_proof_serializes_exactly_as_the_field_order_always_did() {
        // A v1 proof carries the plain per-field encoding; those bytes are hashed into every block
        // and protocol message.
        for (has_pool_pk, has_contract) in
            [(true, false), (false, true), (true, true), (false, false)]
        {
            let pos = make_pos(0, has_pool_pk, has_contract, 32);
            let v = ChiaProtocolVersion::default();
            let mut expected = pos.challenge.to_bytes(v).expect("field");
            expected.extend(pos.pool_public_key.to_bytes(v).expect("field"));
            expected.extend(pos.pool_contract_puzzle_hash.to_bytes(v).expect("field"));
            expected.extend(pos.plot_public_key.to_bytes(v).expect("field"));
            expected.extend(pos.size.to_bytes(v).expect("field"));
            expected.extend(pos.proof.to_bytes(v).expect("field"));
            assert_eq!(pos.to_bytes(v).expect("proof"), expected);
            let parsed = round_trip(&pos);
            assert_eq!(parsed, pos);
        }
    }

    #[test]
    fn a_v2_proof_round_trips_and_drops_size() {
        for (has_pool_pk, has_contract) in [(true, false), (false, true)] {
            let mut pos = make_pos(1, has_pool_pk, has_contract, 0);
            pos.plot_index = 256;
            pos.meta_group = 7;
            pos.strength = 10;
            let parsed = round_trip(&pos);
            assert_eq!(parsed, pos);
            assert_eq!(parsed.version, 1);
            assert_eq!(parsed.size, 0);
        }
    }

    #[test]
    fn the_version_lives_in_the_contract_prefix_byte() {
        let v = ChiaProtocolVersion::default();
        // The prefix sits after the challenge and the pool key Option, so its offset moves with
        // that Option's presence.
        let offset = |has_pool_pk: bool| 32 + 1 + if has_pool_pk { 48 } else { 0 };
        let absent = make_pos(0, true, false, 32).to_bytes(v).expect("proof");
        let present = make_pos(0, true, true, 32).to_bytes(v).expect("proof");
        assert_eq!(
            absent
                .iter()
                .zip(present.iter())
                .position(|(a, b)| a != b)
                .expect("prefix byte differs"),
            offset(true)
        );
        assert_eq!(absent[offset(true)], 0b00);
        assert_eq!(present[offset(true)], 0b01);
        assert_eq!(
            make_pos(1, true, false, 0).to_bytes(v).expect("proof")[offset(true)],
            0b10
        );
        assert_eq!(
            make_pos(1, false, true, 0).to_bytes(v).expect("proof")[offset(false)],
            0b11
        );

        // Any higher bit is malformed for both versions.
        for version in [0u8, 1] {
            for bit in 2..8 {
                let size = if version == 0 { 32 } else { 0 };
                let mut buf = make_pos(version, true, false, size)
                    .to_bytes(v)
                    .expect("proof");
                buf[offset(true)] |= 1 << bit;
                assert!(
                    ProofOfSpace::from_bytes_exact(&buf, v).is_err(),
                    "prefix bit {bit} accepted on version {version}"
                );
            }
        }
    }

    #[test]
    fn a_v2_proof_needs_exactly_one_pool_binding() {
        let v = ChiaProtocolVersion::default();
        // Both set and neither set are rejected at parse; the v1 path stays lenient.
        let both = make_pos(1, true, true, 0).to_bytes(v).expect("proof");
        assert!(ProofOfSpace::from_bytes_exact(&both, v).is_err());
        let neither = make_pos(1, false, false, 0).to_bytes(v).expect("proof");
        assert!(ProofOfSpace::from_bytes_exact(&neither, v).is_err());
        assert!(
            make_pos(2, true, false, 0).to_bytes(v).is_err(),
            "unknown version serialized"
        );
    }

    #[test]
    fn v2_plot_group_ids_match_the_reference_vectors() {
        for (strength, pool, contract, expected) in [
            (
                0u8,
                Some(pool_pk()),
                None,
                "5457cccc4cd79900da4235cf5ca7d978a1993581376e76dfb089c274225419d1",
            ),
            (
                10,
                Some(pool_pk()),
                None,
                "e9d517de0ccfa94baf9e94b39dd0e8afce0451ec27635f43f2aa9b2f429d0501",
            ),
            (
                0,
                None,
                Some(Bytes32::from([1u8; 32])),
                "210d1a307d26acb3fcfa02208061fc6b80e3fbb9ca5f3e4a596b7521d87ccd79",
            ),
            (
                5,
                None,
                Some(Bytes32::from([1u8; 32])),
                "824d7b67ab4269c91eb0a2fe10cb48a1c1ad8cfa8a642387d49d5c3c3acbc3bd",
            ),
        ] {
            assert_eq!(
                calculate_plot_group_id_v2(strength, plot_pk(), pool, contract),
                Bytes32::try_from(expected).expect("hex"),
                "strength {strength}"
            );
        }
    }

    #[test]
    fn v2_plot_ids_match_the_reference_vectors() {
        for (strength, plot_index, meta_group, pool, contract, expected) in [
            (
                0u8,
                0u16,
                0u8,
                Some(pool_pk()),
                None,
                "d3692a5d4fbfe1061053d4afada80d8f0b58b87b46c170e7087716a72091def0",
            ),
            (
                10,
                256,
                7,
                Some(pool_pk()),
                None,
                "2316eadc21d38c4e8740eb9efd49a0c2014a5b1ef992f5ae0b2d1fda01a4b034",
            ),
            (
                0,
                0,
                0,
                None,
                Some(Bytes32::from([1u8; 32])),
                "03b09cab4bfdbcd1e626d93888a72f002d3948459c23cde52e9dd8d72dd9ae04",
            ),
            (
                5,
                100,
                3,
                None,
                Some(Bytes32::from([1u8; 32])),
                "d575860c249ace41a656fe0d97719127f839fae55e6c32ffd7743b5a8a2eae4d",
            ),
        ] {
            assert_eq!(
                calculate_plot_id_v2(strength, plot_pk(), pool, contract, plot_index, meta_group),
                Bytes32::try_from(expected).expect("hex"),
                "strength {strength} index {plot_index}"
            );
        }
    }

    #[test]
    fn v1_is_never_phased_out_before_hard_fork_two() {
        use crate::consensus::constants::MAINNET;
        // MAINNET's HARD_FORK2_HEIGHT is a never-activate sentinel, so no real height triggers it.
        assert!(!is_v1_phased_out(&[0u8; 64], 0, &MAINNET));
        assert!(!is_v1_phased_out(&[0u8; 64], 10_000_000, &MAINNET));
    }

    #[test]
    fn v1_phase_out_retires_more_plots_as_the_cutoff_approaches() {
        use crate::consensus::constants::MAINNET;
        use crate::consensus::overrides::{ConsensusOverrides, apply_overrides};
        // Bring hard fork 2 to a real height with a small epoch so the window is walkable.
        let c = apply_overrides(
            MAINNET,
            &ConsensusOverrides {
                hard_fork2_height: Some(1_000),
                epoch_blocks: Some(100),
                plot_v1_phase_out_epoch_bits: Some(3),
                ..Default::default()
            },
        );
        // 7 epochs wide (mask 7). At the fork height none are retired yet; past the cut-off all are.
        let cut_off = v1_cut_off_height(&c) as u32;
        let retired = |h: u32| {
            (0u16..=255)
                .filter(|i| {
                    let mut proof = vec![0u8; 64];
                    proof[0] = *i as u8;
                    is_v1_phased_out(&proof, h, &c)
                })
                .count()
        };
        let at_fork = retired(1_000);
        let midway = retired((1_000 + cut_off) / 2);
        assert!(
            at_fork < midway,
            "retirement did not grow: {at_fork} then {midway}"
        );
        assert_eq!(
            retired(cut_off + 1),
            256,
            "all v1 plots must be gone past the cut-off"
        );
    }

    #[test]
    fn the_v2_plot_filter_follows_its_own_schedule() {
        use crate::consensus::constants::MAINNET;
        use crate::consensus::overrides::{ConsensusOverrides, apply_overrides};
        let c = apply_overrides(
            MAINNET,
            &ConsensusOverrides {
                plot_filter_v2_first_adjustment_height: Some(1_000),
                plot_filter_v2_second_adjustment_height: Some(2_000),
                plot_filter_v2_third_adjustment_height: Some(3_000),
                ..Default::default()
            },
        );
        assert_eq!(calculate_prefix_bits_v2(&c, 0), 5);
        assert_eq!(calculate_prefix_bits_v2(&c, 999), 5);
        assert_eq!(calculate_prefix_bits_v2(&c, 1_000), 4);
        assert_eq!(calculate_prefix_bits_v2(&c, 2_000), 3);
        assert_eq!(calculate_prefix_bits_v2(&c, 3_000), 2);
        // On stock mainnet constants the schedule has not activated at any real height.
        assert_eq!(calculate_prefix_bits_v2(&MAINNET, 10_000_000), 5);
        // The v1 filter is untouched by the v2 fields.
        assert_eq!(calculate_prefix_bits(&MAINNET, 0), 9);
    }

    #[test]
    fn get_plot_id_routes_by_version() {
        let v2 = make_pos(1, true, false, 0);
        assert_eq!(
            v2.get_plot_id().expect("valid binding"),
            calculate_plot_id_v2(0, plot_pk(), Some(pool_pk()), None, 0, 0)
        );
        let v1 = make_pos(0, true, false, 32);
        assert_eq!(
            v1.get_plot_id().expect("valid binding"),
            calculate_plot_id_public_key(pool_pk(), plot_pk())
        );
        assert!(make_pos(1, true, true, 0).get_plot_id().is_none());
        assert!(make_pos(1, false, false, 0).get_plot_id().is_none());
    }
}
