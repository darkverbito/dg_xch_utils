use crate::blockchain::class_group_element::ClassgroupElement;
use crate::blockchain::sized_bytes::Bytes32;
use dg_xch_macros::ChiaSerial;
use serde::{Deserialize, Serialize};

#[derive(ChiaSerial, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct VdfInfo {
    pub challenge: Bytes32,
    pub number_of_iterations: u64,
    pub output: ClassgroupElement,
}

impl VdfInfo {
    /// chia `VDFInfo(challenge, number_of_iterations, output)`. Constructs a VDF info from its challenge,
    /// iteration count, and output — used by header validation to build reconstructed VDF targets.
    #[must_use]
    pub fn new(challenge: Bytes32, number_of_iterations: u64, output: ClassgroupElement) -> Self {
        Self {
            challenge,
            number_of_iterations,
            output,
        }
    }

    /// chia `info.replace(number_of_iterations=iters)`. Returns a copy of this VDF info with
    /// `number_of_iterations` set to `iters` and the challenge/output unchanged — used by header
    /// validation to build the committed-iteration form of a reconstructed VDF target.
    #[must_use]
    pub fn with_iters(&self, iters: u64) -> Self {
        Self {
            number_of_iterations: iters,
            ..*self
        }
    }
}
