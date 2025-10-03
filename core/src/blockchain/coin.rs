use crate::blockchain::sized_bytes::Bytes32;
use crate::clvm::sexp::SExp;
use dg_xch_macros::ChiaSerial;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sha2::Sha256;
use std::hash::{Hash, Hasher};
use std::io::{Error, ErrorKind};

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct Coin {
    pub parent_coin_info: Bytes32,
    pub puzzle_hash: Bytes32,
    pub amount: u64,
}
impl From<Coin> for SExp {
    fn from(coin: Coin) -> SExp {
        (&coin).into()
    }
}
impl From<&Coin> for SExp {
    fn from(coin: &Coin) -> SExp {
        (&[
            SExp::from(coin.parent_coin_info),
            SExp::from(coin.puzzle_hash),
            SExp::from(coin.amount),
        ])
            .into()
    }
}
impl TryFrom<&SExp> for Coin {
    type Error = Error;
    fn try_from(sexp: &SExp) -> Result<Self, Self::Error> {
        let (parent_coin_info, rest) = sexp.split()?;
        let (puzzle_hash, rest) = rest.split()?;
        let (amount, _) = rest.split()?;
        Ok(Self {
            parent_coin_info: Bytes32::try_from(parent_coin_info)?,
            puzzle_hash: Bytes32::try_from(puzzle_hash)?,
            amount: amount
                .as_int()?
                .to_u64()
                .ok_or(Error::new(ErrorKind::InvalidData, "Invalid prev_subtotal"))?,
        })
    }
}
impl Coin {
    #[must_use]
    pub fn name(&self) -> Bytes32 {
        self.coin_id()
    }
    #[must_use]
    pub fn coin_id(&self) -> Bytes32 {
        let mut hasher = Sha256::new();
        hasher.update(self.parent_coin_info);
        hasher.update(self.puzzle_hash);
        let amount_bytes = self.amount.to_be_bytes();
        if self.amount >= 0x8000_0000_0000_0000_u64 {
            hasher.update([0_u8]);
            hasher.update(amount_bytes);
        } else {
            let start = if self.amount == 0 {
                8
            } else {
                self.amount.leading_zeros().div_ceil(8).saturating_sub(1) as usize
            };
            hasher.update(&amount_bytes[start..]);
        }
        let mut buf = [0u8; 32];
        hasher.finalize_into((&mut buf).into());
        buf.into()
    }
}
impl Hash for Coin {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.name().as_ref());
    }
}
