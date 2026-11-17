//! Wallet-facing BLS keys backed by DG's key derivation and signature provider.
//! Secret keys deliberately do not implement Debug, Display, or Serialize.

use blst::BLST_ERROR;
use dg_xch_core::signatures::bls as signatures;
use dg_xch_puzzles::p2_delegated_puzzle_or_hidden_puzzle as standard;

#[derive(Clone)]
pub struct SecretKey(blst::min_pk::SecretKey);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicKey(blst::min_pk::PublicKey);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature(blst::min_pk::Signature);

impl SecretKey {
    pub fn from_seed(seed: &[u8]) -> Self {
        Self(
            blst::min_pk::SecretKey::key_gen_v3(seed, &[])
                .expect("BLS seed must contain at least 32 bytes"),
        )
    }
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, BLST_ERROR> {
        blst::min_pk::SecretKey::from_bytes(bytes).map(Self)
    }
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.sk_to_pk())
    }
    pub fn native(&self) -> &blst::min_pk::SecretKey {
        &self.0
    }
    pub fn derive_synthetic(&self) -> Self {
        Self(
            standard::calculate_synthetic_secret_key(
                &self.0,
                standard::DEFAULT_HIDDEN_PUZZLE_TREE_HASH,
            )
            .expect("valid BLS key"),
        )
    }
}
impl PublicKey {
    pub fn from_bytes(bytes: &[u8; 48]) -> Result<Self, BLST_ERROR> {
        blst::min_pk::PublicKey::key_validate(bytes).map(Self)
    }
    pub fn to_bytes(&self) -> [u8; 48] {
        self.0.to_bytes()
    }
    pub fn get_fingerprint(&self) -> u32 {
        crate::fingerprint(&self.0)
    }
    pub fn derive_synthetic(&self) -> Self {
        let key = standard::calculate_synthetic_public_key(
            self.to_bytes().into(),
            standard::DEFAULT_HIDDEN_PUZZLE_TREE_HASH,
        )
        .expect("valid BLS key");
        Self::from_bytes(&key.const_bytes()).expect("valid synthetic BLS key")
    }
}
impl Signature {
    pub fn from_bytes(bytes: &[u8; 96]) -> Result<Self, BLST_ERROR> {
        let signature = blst::min_pk::Signature::from_bytes(bytes)?;
        signature.validate(false)?;
        Ok(Self(signature))
    }
    pub fn to_bytes(&self) -> [u8; 96] {
        self.0.to_bytes()
    }
}
impl Default for Signature {
    fn default() -> Self {
        let mut bytes = [0; 96];
        bytes[0] = 0xc0;
        Self::from_bytes(&bytes).expect("G2 identity")
    }
}
pub trait WalletKey: Sized {
    fn wallet_unhardened(&self, index: u32) -> Self;
}
impl WalletKey for SecretKey {
    fn wallet_unhardened(&self, index: u32) -> Self {
        Self(
            crate::master_sk_to_wallet_sk_unhardened(&self.0, index).expect("valid BLS derivation"),
        )
    }
}
impl WalletKey for PublicKey {
    fn wallet_unhardened(&self, index: u32) -> Self {
        let mut key = self.0;
        for i in [12381, 8444, 2, index] {
            key = crate::derive_child_pk_unhardened(&key, i).expect("valid BLS derivation");
        }
        Self(key)
    }
}
pub fn master_to_wallet_unhardened<K: WalletKey>(key: &K, index: u32) -> K {
    key.wallet_unhardened(index)
}
pub fn master_to_wallet_hardened(key: &SecretKey, index: u32) -> SecretKey {
    SecretKey(crate::master_sk_to_wallet_sk(key.native(), index).expect("valid BLS derivation"))
}
pub fn sign(key: &SecretKey, message: impl AsRef<[u8]>) -> Signature {
    Signature(signatures::sign(key.native(), message.as_ref()))
}
pub fn verify(signature: &Signature, key: &PublicKey, message: impl AsRef<[u8]>) -> bool {
    signatures::verify_signature(&key.0, message.as_ref(), &signature.0)
}
pub fn aggregate(signatures: &[Signature]) -> Signature {
    let bytes = signatures
        .iter()
        .map(|s| s.to_bytes().into())
        .collect::<Vec<_>>();
    let result = signatures::aggregate_signatures(&bytes).expect("validated BLS signatures");
    Signature::from_bytes(&result.const_bytes()).expect("BLS aggregate")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chia_seed_vectors_and_public_derivation() {
        for (seed, expected) in [
            (
                "fc795be0c3f18c50dddb34e72179dc597d64055497ecc1e69e2e56a5409651bc139aae8070d4df0ea14d8d2a518a9a00bb1cc6e92e053fe34051f6821df9164c",
                "52d75c4707e39595b27314547f9723e5530c01198af3fc5849d9a7af65631efb",
            ),
            (
                "b873212f885ccffbf4692afcb84bc2e55886de2dfa07d90f5c3c239abc31c0a6ce047e30fd8bf6a281e71389aa82d73df74c7bbfb3b06b4639a5cee775cccd3c",
                "35d65c35d926f62ba2dd128754ddb556edb4e2c926237ab9e02a23e7b3533613",
            ),
        ] {
            let key = SecretKey::from_seed(&hex::decode(seed).unwrap());
            assert_eq!(hex::encode(key.to_bytes()), expected);
            for index in [0, 1, 7, 99] {
                let child = master_to_wallet_unhardened(&key, index);
                assert_eq!(
                    child.public_key(),
                    master_to_wallet_unhardened(&key.public_key(), index)
                );
                assert_eq!(
                    child.derive_synthetic().public_key(),
                    child.public_key().derive_synthetic()
                );
            }
        }
    }
    #[test]
    fn chia_chip0002_vector() {
        let key = PublicKey::from_bytes(&hex::decode("8e156d106f1b0ff0ebbe5ab27b1797a19cf3e895a7a435b003a1df2dd477d622be928379625b759ef3b388b286ee8658").unwrap().try_into().unwrap()).unwrap();
        let sig = Signature::from_bytes(&hex::decode("a804111f80be2ed0d4d3fdd139c8fe20cd506b99b03592563d85292abcbb9cd6ff6df2e7a13093e330d66aa5218bbe0e17677c9a23a9f18dbe488b7026be59d476161f5e6f0eea109cd7be22b1f74fda9c80c6b845ecc691246eb1c7f1b66a6a").unwrap().try_into().unwrap()).unwrap();
        use dg_xch_core::clvm::program::Program;
        let hash = Program::to((
            b"Chia Signed Message".to_vec(),
            b"4861707079206861707079206a6f79206a6f79".to_vec(),
        ))
        .tree_hash();
        assert!(verify(&sig, &key, hash));
        assert!(!verify(&sig, &key, [0; 32]));
        let ph = standard::puzzle_hash_for_synthetic_public_key(key.to_bytes().into()).unwrap();
        assert_eq!(
            crate::encode_puzzle_hash(&ph, "xch").unwrap(),
            "xch1e2pcue5q7t4sg8gygz3aht369sk78rzzs92zx65ktn9a9qurw35saajvkh"
        );
        assert!(PublicKey::from_bytes(&[0; 48]).is_err());
        assert!(Signature::from_bytes(&[0; 96]).is_err());
    }
}
