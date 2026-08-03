#![allow(dead_code)]

use std::io::Cursor;
use std::path::PathBuf;

use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn load_fixture() -> WeightProof {
    let data = std::fs::read(fixtures_dir().join("weight_proof_mainnet_9054698.bin"))
        .expect("mainnet weight-proof fixture present");
    let mut cur = Cursor::new(data.as_slice());
    WeightProof::from_bytes(&mut cur, ChiaProtocolVersion::default())
        .expect("real mainnet weight proof deserializes")
}
