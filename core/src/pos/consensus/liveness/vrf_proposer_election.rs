// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::pos::consensus::liveness::proposer_election::ProposerElection;
use consensus_types::common::{Author, Round};

use cfx_types::U256;
use consensus_types::block::{Block, VRF_SEED};
use diem_crypto::VRFProof;
use parking_lot::Mutex;
use std::collections::HashMap;

/// FIXME(lpl): Set by validator count.
pub const PROPOSAL_THRESHOLD: U256 = U256::MAX;

/// The round proposer maps a round to author
pub struct VrfProposer {
    author: Author,
    current_round: Mutex<Round>,
    current_seed: Mutex<Vec<u8>>,
    proposal_candidates: Mutex<Vec<Block>>,
}

impl VrfProposer {
    pub fn new(author: Author) -> Self {
        Self {
            author,
            current_round: Mutex::new(0),
            current_seed: Mutex::new(VRF_SEED.to_vec()),
            proposal_candidates: Default::default(),
        }
    }
}

impl ProposerElection for VrfProposer {
    fn get_valid_proposer(&self, _round: Round) -> Author {
        unreachable!(
            "We will never get valid proposer based on round for VRF election"
        )
    }

    fn is_valid_proposer(&self, author: Author, round: Round) -> bool {
        assert_eq!(author, self.author, "VRF election can not check proposer validity without vrf_proof");
        let vrf_proof =
    }

    fn is_valid_proposal(&self, block: &Block) -> bool {
        let vrf_number = U256::from_big_endian(
            block.vrf_proof().unwrap().to_hash().unwrap().as_ref(),
        );
        vrf_number <= PROPOSAL_THRESHOLD
    }

    fn is_random_election(&self) -> bool { true }

    fn receive_proposal_candidate(&self, block: Block) -> bool {
        if self.is_valid_proposal(&block)
            && block.round() == *self.current_round.lock()
        {
            self.proposal_candidates.lock().push(block);
            true
        } else {
            false
        }
    }

    /// Choose a proposal from all received proposal candidates to vote for.
    fn choose_proposal_to_vote(&self) -> Option<Block> {
        let mut chosen_proposal = None;
        let mut min_vrf_number = U256::MAX;
        for b in &*self.proposal_candidates.lock() {
            let vrf_number = U256::from_big_endian(
                b.vrf_proof().unwrap().to_hash().unwrap().as_ref(),
            );
            if vrf_number < min_vrf_number {
                chosen_proposal = Some(b.clone());
                min_vrf_number = vrf_number
            }
        }
        chosen_proposal
    }

    fn next_round(&self, round: Round) {
        *self.current_round.lock() = round;
        self.proposal_candidates.lock().clear();
    }
}
