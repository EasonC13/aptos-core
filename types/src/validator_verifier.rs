// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{account_address::AccountAddress, on_chain_config::ValidatorSet};
use aptos_crypto::{bls12381, hash::CryptoHash, Signature, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};
use thiserror::Error;

use crate::multi_signature::{MultiSignature, PartialSignatures};
#[cfg(any(test, feature = "fuzzing"))]
use crate::validator_signer::ValidatorSigner;
use anyhow::{ensure, Result};
use itertools::Itertools;
use aptos_crypto::bls12381::PublicKey;
#[cfg(any(test, feature = "fuzzing"))]
use proptest_derive::Arbitrary;

/// Errors possible during signature verification.
#[derive(Debug, Error, PartialEq)]
pub enum VerifyError {
    #[error("Author is unknown")]
    /// The author for this signature is unknown by this validator.
    UnknownAuthor,
    #[error(
        "The voting power ({}) is less than expected voting power ({})",
        voting_power,
        expected_voting_power
    )]
    TooLittleVotingPower {
        voting_power: u128,
        expected_voting_power: u128,
    },
    #[error(
        "The number of voters ({}) is greater than total number of authors ({})",
        num_of_voters,
        num_of_authors
    )]
    TooManyVoters {
        num_of_voters: usize,
        num_of_authors: usize,
    },
    #[error("Signature is empty")]
    /// The signature is empty
    EmptySignature,
    #[error("Signature is invalid")]
    /// The signature is invalid
    InvalidSignature,
    #[error("Inconsistent Block Info")]
    InconsistentBlockInfo,
    #[error("Failed to aggregate public keys")]
    FailedToAggregatePubKey,
    #[error("Failed to aggregate signatures")]
    FailedToAggregateSignature,
    #[error("Failed to verify multi-signature")]
    FailedToVerifyMultiSignature,
}

/// Helper struct to manage validator information for validation
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct ValidatorConsensusInfo {
    address: AccountAddress,
    public_key: PublicKey,
    voting_power: u64,
}

impl ValidatorConsensusInfo {
    pub fn new(address: AccountAddress, public_key: PublicKey, voting_power: u64) -> Self {
        ValidatorConsensusInfo {
            address,
            public_key,
            voting_power,
        }
    }

    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

/// Supports validation of signatures for known authors with individual voting powers. This struct
/// can be used for all signature verification operations including block and network signature
/// verification, respectively.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidatorVerifier {
    /// A vector of each validator's on-chain account address to its pubkeys and voting power.
    validator_infos: Vec<ValidatorConsensusInfo>,
    /// The minimum voting power required to achieve a quorum
    #[serde(skip)]
    quorum_voting_power: u128,
    /// Total voting power of all validators (cached from address_to_validator_info)
    #[serde(skip)]
    total_voting_power: u128,
    /// In-memory index of account address to its index in the vector, does not go through serde.
    #[serde(skip)]
    address_to_validator_index: HashMap<AccountAddress, usize>,
}

/// Reconstruct fields from the raw data upon deserialization.
impl<'de> Deserialize<'de> for ValidatorVerifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename = "ValidatorVerifier")]
        struct RawValidatorVerifier {
            validator_infos: Vec<ValidatorConsensusInfo>,
        }

        let RawValidatorVerifier { validator_infos } =
            RawValidatorVerifier::deserialize(deserializer)?;

        Ok(ValidatorVerifier::new(validator_infos))
    }
}

impl ValidatorVerifier {
    /// Private constructor to calculate the in-memory index
    fn build_index(
        validator_infos: Vec<ValidatorConsensusInfo>,
        quorum_voting_power: u128,
        total_voting_power: u128,
    ) -> Self {
        let address_to_validator_index = validator_infos
            .iter()
            .enumerate()
            .map(|(index, info)| (info.address, index))
            .collect();
        Self {
            validator_infos,
            quorum_voting_power,
            total_voting_power,
            address_to_validator_index,
        }
    }

    /// Initialize with a map of account address to validator info and set quorum size to
    /// default (`2f + 1`) or zero if `address_to_validator_info` is empty.
    pub fn new(validator_infos: Vec<ValidatorConsensusInfo>) -> Self {
        let total_voting_power = sum_voting_power(&validator_infos);
        let quorum_voting_power = if validator_infos.is_empty() {
            0
        } else {
            total_voting_power * 2 / 3 + 1
        };
        Self::build_index(validator_infos, quorum_voting_power, total_voting_power)
    }

    /// Initializes a validator verifier with a specified quorum voting power.
    pub fn new_with_quorum_voting_power(
        validator_infos: Vec<ValidatorConsensusInfo>,
        quorum_voting_power: u128,
    ) -> Result<Self> {
        let total_voting_power = sum_voting_power(&validator_infos);
        ensure!(
            quorum_voting_power <= total_voting_power,
            "Quorum voting power is greater than the sum of all voting power of authors: {}, \
             quorum_size: {}.",
            quorum_voting_power,
            total_voting_power
        );
        Ok(Self::build_index(
            validator_infos,
            quorum_voting_power,
            total_voting_power,
        ))
    }

    /// Helper method to initialize with a single author and public key with quorum voting power 1.
    pub fn new_single(author: AccountAddress, public_key: PublicKey) -> Self {
        let validator_infos = vec![ValidatorConsensusInfo::new(author, public_key, 1)];
        Self::new(validator_infos)
    }

    /// Verify the correctness of a signature of a message by a known author.
    pub fn verify<T: Serialize + CryptoHash>(
        &self,
        author: AccountAddress,
        message: &T,
        signature: &bls12381::Signature,
    ) -> std::result::Result<(), VerifyError> {
        match self.get_public_key(&author) {
            Some(public_key) => public_key
                .verify_struct_signature(message, signature)
                .map_err(|_| VerifyError::InvalidSignature),
            None => Err(VerifyError::UnknownAuthor),
        }
    }

    // Generates a multi signature from partial signatures without actually verifying it.
    pub fn aggregate_multi_signature(
        &self,
        partial_signatures: &PartialSignatures,
    ) -> Result<(MultiSignature, PublicKey), VerifyError> {
        let mut pub_keys = vec![];
        let mut sigs = vec![];
        let mut masks = vec![false; self.validator_infos.len()];
        for (addr, sig) in partial_signatures.signatures() {
            let index = *self
                .address_to_validator_index
                .get(addr)
                .ok_or(VerifyError::UnknownAuthor)?;
            masks[index] = true;
            pub_keys.push(self.validator_infos[index].public_key());
            sigs.push(sig.clone());
        }
        // Perform an optimistic aggregation of the signatures without verification.
        let aggregated_sig = bls12381::Signature::aggregate(sigs)
            .map_err(|_| VerifyError::FailedToAggregateSignature)?;

        let aggregated_key =
            PublicKey::aggregate(pub_keys).map_err(|_| VerifyError::FailedToAggregatePubKey)?;
        Ok((
            MultiSignature::new(masks, Some(aggregated_sig)),
            aggregated_key,
        ))
    }

    pub fn aggregate_and_verify_multi_signature<T: CryptoHash + Serialize>(
        &self,
        partial_signatures: &PartialSignatures,
        message: &T,
    ) -> Result<MultiSignature, VerifyError> {
        let (aggregated_sig, aggregated_key) =
            self.aggregate_multi_signature(partial_signatures)?;
        // Verify the multi-signature
        aggregated_sig
            .multi_sig()
            .as_ref()
            .expect("Failed to get multi signature")
            .verify(message, &aggregated_key)
            .map_err(|_| VerifyError::FailedToVerifyMultiSignature)?;
        Ok(aggregated_sig)
    }

    pub fn validator_addresses(&self) -> Vec<AccountAddress> {
        self.get_ordered_account_addresses_iter().collect_vec()
    }

    /// This function will successfully return when at least quorum_size signatures of known authors
    /// are successfully verified. It creates an aggregated public key using the voter bitmask passed
    /// in the multi-signature and verifies the message passed in the multi-signature using the aggregated
    /// public key.
    ///
    pub fn verify_multi_signatures<T: CryptoHash + Serialize>(
        &self,
        message: &T,
        multi_signature: &MultiSignature,
    ) -> std::result::Result<(), VerifyError> {
        // Verify the number of signature is not greater than expected.
        self.check_num_of_voters(multi_signature)?;
        let mut pub_keys = vec![];
        let mut authors = vec![];
        for (index, exist) in multi_signature.get_voters_bitmap().iter().enumerate() {
            if *exist {
                authors.push(self.validator_infos[index].address);
                pub_keys.push(self.validator_infos[index].public_key());
            }
        }
        // Verify the quorum voting power of the authors
        self.check_voting_power(authors.iter())?;
        #[cfg(any(test, feature = "fuzzing"))]
        {
            if self.quorum_voting_power == 0 {
                // This should happen only in case of tests.
                // TODO(skedia): Clean up the test behaviors to not rely on empty signature
                // verification
                return Ok(());
            }
        }
        // Verify empty multi signature
        let multi_sig = multi_signature
            .multi_sig()
            .as_ref()
            .ok_or(VerifyError::EmptySignature)?;
        // Verify the optimistically aggregated signature.
        let aggregated_key =
            PublicKey::aggregate(pub_keys).map_err(|_| VerifyError::FailedToAggregatePubKey)?;

        multi_sig
            .verify(message, &aggregated_key)
            .map_err(|_| VerifyError::InvalidSignature)?;
        Ok(())
    }

    /// Ensure there are not more than the maximum expected voters (all possible signatures).
    fn check_num_of_voters(
        &self,
        multi_signature: &MultiSignature,
    ) -> std::result::Result<(), VerifyError> {
        let num_of_voters = multi_signature.get_num_voters();
        if num_of_voters > self.len() {
            return Err(VerifyError::TooManyVoters {
                num_of_voters,
                num_of_authors: self.len(),
            });
        }
        Ok(())
    }

    /// Ensure there is at least quorum_voting_power in the provided signatures and there
    /// are only known authors. According to the threshold verification policy,
    /// invalid public keys are not allowed.
    pub fn check_voting_power<'a>(
        &self,
        authors: impl Iterator<Item = &'a AccountAddress>,
    ) -> std::result::Result<(), VerifyError> {
        // Add voting power for valid accounts, exiting early for unknown authors
        let mut aggregated_voting_power = 0;
        for account_address in authors {
            match self.get_voting_power(account_address) {
                Some(voting_power) => aggregated_voting_power += voting_power as u128,
                None => return Err(VerifyError::UnknownAuthor),
            }
        }

        if aggregated_voting_power < self.quorum_voting_power {
            return Err(VerifyError::TooLittleVotingPower {
                voting_power: aggregated_voting_power,
                expected_voting_power: self.quorum_voting_power,
            });
        }
        Ok(())
    }

    /// Returns the public key for this address.
    pub fn get_public_key(&self, author: &AccountAddress) -> Option<PublicKey> {
        self.address_to_validator_index
            .get(author)
            .map(|index| self.validator_infos[*index].public_key().clone())
    }

    /// Returns the voting power for this address.
    pub fn get_voting_power(&self, author: &AccountAddress) -> Option<u64> {
        self.address_to_validator_index
            .get(author)
            .map(|index| self.validator_infos[*index].voting_power)
    }

    /// Returns an ordered list of account addresses as an `Iterator`.
    pub fn get_ordered_account_addresses_iter(&self) -> impl Iterator<Item = AccountAddress> + '_ {
        self.validator_infos.iter().map(|info| info.address)
    }

    /// Returns the number of authors to be validated.
    pub fn len(&self) -> usize {
        self.validator_infos.len()
    }

    /// Is there at least one author?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns quorum voting power.
    pub fn quorum_voting_power(&self) -> u128 {
        self.quorum_voting_power
    }

    /// Returns total voting power.
    pub fn total_voting_power(&self) -> u128 {
        self.total_voting_power
    }
}

/// Returns sum of voting power from Map of validator account addresses, validator consensus info
fn sum_voting_power(address_to_validator_info: &[ValidatorConsensusInfo]) -> u128 {
    address_to_validator_info.iter().fold(0, |sum, x| {
        sum.checked_add(x.voting_power as u128)
            .expect("sum of all voting power is greater than u64::max")
    })
}

impl fmt::Display for ValidatorVerifier {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        write!(f, "ValidatorSet: [")?;
        for info in &self.validator_infos {
            write!(
                f,
                "{}: {}, ",
                info.address.short_str_lossless(),
                info.voting_power
            )?;
        }
        write!(f, "]")
    }
}

/// This does the conversion between move data to the rust data
impl From<&ValidatorSet> for ValidatorVerifier {
    fn from(validator_set: &ValidatorSet) -> Self {
        let sorted_validator_infos: BTreeMap<u64, ValidatorConsensusInfo> = validator_set
            .payload()
            .map(|info| {
                (
                    info.config().validator_index,
                    ValidatorConsensusInfo::new(
                        info.account_address,
                        info.consensus_public_key().clone(),
                        info.consensus_voting_power(),
                    ),
                )
            })
            .collect();
        let validator_infos: Vec<_> = sorted_validator_infos.values().cloned().collect();
        for info in validator_set.payload() {
            assert_eq!(
                validator_infos[info.config().validator_index as usize].address,
                info.account_address
            );
        }
        ValidatorVerifier::new(validator_infos)
    }
}

#[cfg(any(test, feature = "fuzzing"))]
impl From<&ValidatorVerifier> for ValidatorSet {
    fn from(verifier: &ValidatorVerifier) -> Self {
        ValidatorSet::new(
            verifier
                .get_ordered_account_addresses_iter()
                .enumerate()
                .map(|(index, addr)| {
                    crate::validator_info::ValidatorInfo::new_with_test_network_keys(
                        addr,
                        verifier.get_public_key(&addr).unwrap(),
                        verifier.get_voting_power(&addr).unwrap(),
                        index as u64,
                    )
                })
                .collect(),
        )
    }
}

/// Helper function to generate LedgerInfoWithSignature from a set of validator signers used for testing
#[cfg(any(test, feature = "fuzzing"))]
pub fn generate_validator_verifier(validators: &[ValidatorSigner]) -> ValidatorVerifier {
    let validator_consensus_info = validators
        .iter()
        .map(|signer| ValidatorConsensusInfo::new(signer.author(), signer.public_key(), 1))
        .collect();

    ValidatorVerifier::new_with_quorum_voting_power(
        validator_consensus_info,
        validators.len() as u128 / 2,
    )
    .expect("Incorrect quorum size.")
}

/// Helper function to get random validator signers and a corresponding validator verifier for
/// testing.  If custom_voting_power_quorum is not None, set a custom voting power quorum amount.
/// With pseudo_random_account_address enabled, logs show 0 -> [0000], 1 -> [1000]
#[cfg(any(test, feature = "fuzzing"))]
pub fn random_validator_verifier(
    count: usize,
    custom_voting_power_quorum: Option<u128>,
    pseudo_random_account_address: bool,
) -> (Vec<ValidatorSigner>, ValidatorVerifier) {
    let mut signers = Vec::new();
    let mut validator_infos = vec![];
    for i in 0..count {
        let random_signer = if pseudo_random_account_address {
            ValidatorSigner::from_int(i as u8)
        } else {
            ValidatorSigner::random([i as u8; 32])
        };
        validator_infos.push(ValidatorConsensusInfo::new(
            random_signer.author(),
            random_signer.public_key(),
            1,
        ));
        signers.push(random_signer);
    }
    (
        signers,
        match custom_voting_power_quorum {
            Some(custom_voting_power_quorum) => ValidatorVerifier::new_with_quorum_voting_power(
                validator_infos,
                custom_voting_power_quorum,
            )
            .expect("Unable to create testing validator verifier"),
            None => ValidatorVerifier::new(validator_infos),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator_signer::ValidatorSigner;
    use aptos_crypto::test_utils::{TestAptosCrypto, TEST_SEED};
    use std::collections::{BTreeMap, HashMap};

    #[test]
    fn test_check_voting_power() {
        let (validator_signers, validator_verifier) = random_validator_verifier(2, None, false);
        let mut author_to_signature_map = BTreeMap::new();

        assert_eq!(
            validator_verifier
                .check_voting_power(author_to_signature_map.keys())
                .unwrap_err(),
            VerifyError::TooLittleVotingPower {
                voting_power: 0,
                expected_voting_power: 2,
            }
        );

        let dummy_struct = TestAptosCrypto("Hello, World".to_string());
        for validator in validator_signers.iter() {
            author_to_signature_map.insert(validator.author(), validator.sign(&dummy_struct));
        }

        assert_eq!(
            validator_verifier.check_voting_power(author_to_signature_map.keys()),
            Ok(())
        );
    }

    #[test]
    fn test_validator() {
        let validator_signer = ValidatorSigner::random(TEST_SEED);
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());
        let signature = validator_signer.sign(&dummy_struct);
        let validator =
            ValidatorVerifier::new_single(validator_signer.author(), validator_signer.public_key());
        assert_eq!(
            validator.verify(validator_signer.author(), &dummy_struct, &signature),
            Ok(())
        );
        let unknown_validator_signer = ValidatorSigner::random([1; 32]);
        let unknown_signature = unknown_validator_signer.sign(&dummy_struct);
        assert_eq!(
            validator.verify(
                unknown_validator_signer.author(),
                &dummy_struct,
                &unknown_signature
            ),
            Err(VerifyError::UnknownAuthor)
        );
        assert_eq!(
            validator.verify(validator_signer.author(), &dummy_struct, &unknown_signature),
            Err(VerifyError::InvalidSignature)
        );
    }

    #[test]
    fn test_invalid_multi_signatures() {
        let validator_signer = ValidatorSigner::random(TEST_SEED);
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());
        let validator =
            ValidatorVerifier::new_single(validator_signer.author(), validator_signer.public_key());

        // Generate a multi-sig from invalid signer and ensure verify_mutli_signatures fails.
        let unknown_validator_signer = ValidatorSigner::random([1; 32]);
        let unknown_signature = unknown_validator_signer.sign(&dummy_struct);
        let unknown_validator = ValidatorVerifier::new_single(
            unknown_validator_signer.author(),
            unknown_validator_signer.public_key(),
        );
        let mut partial_sig = PartialSignatures::empty();
        partial_sig.add_signature(unknown_validator_signer.author(), unknown_signature);

        let (multi_sig, _) = unknown_validator
            .aggregate_multi_signature(&partial_sig)
            .unwrap();

        assert_eq!(
            validator.verify_multi_signatures(&dummy_struct, &multi_sig),
            Err(VerifyError::InvalidSignature)
        );
    }

    #[test]
    fn test_verify_empty_signature() {
        let validator_signer = ValidatorSigner::random(TEST_SEED);
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());
        let validator =
            ValidatorVerifier::new_single(validator_signer.author(), validator_signer.public_key());

        assert_eq!(
            validator
                .verify_multi_signatures(&dummy_struct, &MultiSignature::new(vec![true], None)),
            Err(VerifyError::EmptySignature)
        );
    }

    #[test]
    fn test_insufficient_voting_power() {
        let validator_signer = ValidatorSigner::random(TEST_SEED);
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());
        let validator =
            ValidatorVerifier::new_single(validator_signer.author(), validator_signer.public_key());

        assert_eq!(
            // This should fail with insufficient quorum voting power.
            validator.verify_multi_signatures(&dummy_struct, &MultiSignature::empty()),
            Err(VerifyError::TooLittleVotingPower {
                voting_power: 0,
                expected_voting_power: 1
            })
        );
    }

    #[test]
    fn test_equal_vote_quorum_validators() {
        const NUM_SIGNERS: u8 = 7;
        // Generate NUM_SIGNERS random signers.
        let validator_signers: Vec<ValidatorSigner> = (0..NUM_SIGNERS)
            .map(|i| ValidatorSigner::random([i; 32]))
            .collect();
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());

        // Create a map from authors to public keys with equal voting power.
        let mut validator_infos = vec![];
        for validator in validator_signers.iter() {
            validator_infos.push(ValidatorConsensusInfo::new(
                validator.author(),
                validator.public_key(),
                1,
            ));
        }

        // Create a map from author to signatures.
        let mut partial_signature = PartialSignatures::new(HashMap::new());
        for validator in validator_signers.iter() {
            partial_signature.add_signature(validator.author(), validator.sign(&dummy_struct));
        }

        // Let's assume our verifier needs to satisfy at least 5 signatures from the original
        // NUM_SIGNERS.
        let validator_verifier =
            ValidatorVerifier::new_with_quorum_voting_power(validator_infos, 5)
                .expect("Incorrect quorum size.");

        let mut aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;
        // Check against signatures == N; this will pass.
        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Ok(())
        );

        // Add an extra unknown signer, signatures > N; this will fail.
        let unknown_validator_signer = ValidatorSigner::random([NUM_SIGNERS + 1; 32]);
        let unknown_signature = unknown_validator_signer.sign(&dummy_struct);
        partial_signature
            .add_signature(unknown_validator_signer.author(), unknown_signature.clone());

        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );

        // Add 5 valid signers only (quorum threshold is met); this will pass.
        partial_signature = PartialSignatures::new(HashMap::new());
        for validator in validator_signers.iter().take(5) {
            partial_signature.add_signature(validator.author(), validator.sign(&dummy_struct));
        }
        aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;
        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Ok(())
        );

        // Add an unknown signer, but quorum is satisfied and signatures <= N; this will fail as we
        // don't tolerate invalid signatures.
        partial_signature
            .add_signature(unknown_validator_signer.author(), unknown_signature.clone());

        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );

        // Add 4 valid signers only (quorum threshold is NOT met); this will fail.
        partial_signature = PartialSignatures::new(HashMap::new());
        for validator in validator_signers.iter().take(4) {
            partial_signature.add_signature(validator.author(), validator.sign(&dummy_struct));
        }
        aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;
        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Err(VerifyError::TooLittleVotingPower {
                voting_power: 4,
                expected_voting_power: 5
            })
        );

        // Add an unknown signer, we have 5 signers, but one of them is invalid; this will fail.
        partial_signature.add_signature(unknown_validator_signer.author(), unknown_signature);
        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );
    }

    #[test]
    fn test_unequal_vote_quorum_validators() {
        const NUM_SIGNERS: u8 = 4;
        // Generate NUM_SIGNERS random signers.
        let validator_signers: Vec<ValidatorSigner> = (0..NUM_SIGNERS)
            .map(|i| ValidatorSigner::random([i; 32]))
            .collect();
        let dummy_struct = TestAptosCrypto("Hello, World".to_string());

        // Create a map from authors to public keys with increasing weights (0, 1, 2, 3) and
        // a map of author to signature.
        let mut validator_infos = vec![];
        let mut partial_signature = PartialSignatures::new(HashMap::new());
        for (i, validator_signer) in validator_signers.iter().enumerate() {
            validator_infos.push(ValidatorConsensusInfo::new(
                validator_signer.author(),
                validator_signer.public_key(),
                i as u64,
            ));
            partial_signature.add_signature(
                validator_signer.author(),
                validator_signer.sign(&dummy_struct),
            );
        }

        // Let's assume our verifier needs to satisfy at least 5 quorum voting power
        let validator_verifier =
            ValidatorVerifier::new_with_quorum_voting_power(validator_infos, 5)
                .expect("Incorrect quorum size.");

        let mut aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;

        // Check against all signatures (6 voting power); this will pass.
        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Ok(())
        );

        // Add an extra unknown signer, signatures > N; this will fail.
        let unknown_validator_signer = ValidatorSigner::random([NUM_SIGNERS + 1; 32]);
        let unknown_signature = unknown_validator_signer.sign(&dummy_struct);
        partial_signature
            .add_signature(unknown_validator_signer.author(), unknown_signature.clone());

        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );

        // Add 5 voting power signers only (quorum threshold is met) with (2, 3) ; this will pass.
        let mut partial_signature = PartialSignatures::new(HashMap::new());
        for validator in validator_signers.iter().skip(2) {
            partial_signature.add_signature(validator.author(), validator.sign(&dummy_struct));
        }

        aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;

        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Ok(())
        );

        // Add an unknown signer, but quorum is satisfied and signatures <= N; this will fail as we
        // don't tolerate invalid signatures.
        partial_signature
            .add_signature(unknown_validator_signer.author(), unknown_signature.clone());
        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );

        // Add first 3 valid signers only (quorum threshold is NOT met); this will fail.
        let mut partial_signature = PartialSignatures::new(HashMap::new());
        for validator in validator_signers.iter().take(3) {
            partial_signature.add_signature(validator.author(), validator.sign(&dummy_struct));
        }
        aggregated_signature = validator_verifier
            .aggregate_multi_signature(&partial_signature)
            .unwrap()
            .0;
        assert_eq!(
            validator_verifier.verify_multi_signatures(&dummy_struct, &aggregated_signature),
            Err(VerifyError::TooLittleVotingPower {
                voting_power: 3,
                expected_voting_power: 5
            })
        );

        // Add an unknown signer, we have 5 signers, but one of them is invalid; this will fail.
        partial_signature.add_signature(unknown_validator_signer.author(), unknown_signature);
        assert_eq!(
            validator_verifier.aggregate_multi_signature(&partial_signature),
            Err(VerifyError::UnknownAuthor)
        );
    }
}