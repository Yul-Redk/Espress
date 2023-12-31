// Copyright (c) 2022 Espresso Systems (espressosys.com)
// This file is part of the Espresso library.

use crate::stake_table::StakingKey;
use crate::{
    state::{ArcSer, ChainVariables},
    universal_params::MERKLE_HEIGHT,
    util::canonical,
};
use arbitrary::{Arbitrary, Unstructured};
use arbitrary_wrappers::ArbitraryRecordOpening;
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Read, SerializationError, Write};
use async_std::sync::Arc;
use commit::{Commitment, Committable, RawCommitmentBuilder};
use espresso_macros::ser_test;
use jf_cap::structs::Amount;
use jf_cap::{
    structs::{RecordCommitment, RecordOpening},
    MerkleTree,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Genesis transaction
///
/// A genesis transaction is used to initialize the Espresso ledger, setting per-chain variables and
/// populating the ledger with some initial records. It is valid in exactly one situation: when it
/// is the only transaction in the genesis block, block number 0. In this case, it has the effect of
/// setting the chain variables to `chain` and seeding the record set with commitments to
/// `faucet_records`.
#[ser_test(arbitrary)]
#[derive(
    Clone,
    Debug,
    CanonicalSerialize,
    CanonicalDeserialize,
    Serialize,
    Deserialize,
    Hash,
    PartialEq,
    Eq,
)]
pub struct GenesisNote {
    pub chain: ChainVariables,
    pub faucet_records: ArcSer<Vec<RecordOpening>>,
    pub stake_table: BTreeMap<StakingKey, Amount>,
}

impl Committable for GenesisNote {
    fn commit(&self) -> Commitment<Self> {
        RawCommitmentBuilder::new("GenesisNote")
            .field("chain", self.chain.commit())
            .var_size_bytes(&canonical::serialize(&self.faucet_records).unwrap())
            .finalize()
    }
}

impl<'a> Arbitrary<'a> for GenesisNote {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self {
            chain: u.arbitrary()?,
            faucet_records: ArcSer::new(
                u.arbitrary_iter::<ArbitraryRecordOpening>()?
                    .map(|ro| Ok(ro?.into()))
                    .collect::<Result<_, _>>()?,
            ),
            stake_table: u
                .arbitrary_iter::<(StakingKey, u64)>()?
                .map(|res| {
                    let (key, amt) = res?;
                    Ok((key, Amount::from(amt)))
                })
                .collect::<Result<_, _>>()?,
        })
    }
}

impl GenesisNote {
    pub fn new(
        chain: ChainVariables,
        faucet_records: Arc<Vec<RecordOpening>>,
        stake_table: BTreeMap<StakingKey, Amount>,
    ) -> Self {
        Self {
            chain,
            faucet_records: faucet_records.into(),
            stake_table,
        }
    }
    pub fn output_len(&self) -> usize {
        self.faucet_records.len()
    }

    pub fn output_commitments(&self) -> Vec<RecordCommitment> {
        self.faucet_records
            .iter()
            .map(RecordCommitment::from)
            .collect()
    }

    pub fn output_openings(&self) -> Vec<RecordOpening> {
        (**self.faucet_records).clone()
    }

    pub fn record_merkle_tree(&self) -> MerkleTree {
        let mut records = MerkleTree::new(MERKLE_HEIGHT).unwrap();
        for comm in self.output_commitments() {
            records.push(comm.to_field_element());
        }
        records
    }
}
