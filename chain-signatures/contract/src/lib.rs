pub mod primitives;

use crypto_shared::{
    derive_epsilon, derive_key, kdf::check_ec_signature, near_public_key_to_affine_point,
    types::SignatureResponse, ScalarExt as _, SerializableScalar,
};
use k256::Scalar;
use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::collections::LookupMap;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::FunctionError;

use near_sdk::{
    env, log, near_bindgen, AccountId, BorshStorageKey, CryptoHash, Gas, GasWeight, NearToken,
    PromiseError, PublicKey,
};

use k256::elliptic_curve::sec1::ToEncodedPoint;
use primitives::{
    CandidateInfo, Candidates, ParticipantInfo, Participants, PkVotes, SignRequest,
    SignaturePromiseError, SignatureResult, Votes,
};
use std::collections::{BTreeMap, HashSet};

const GAS_FOR_SIGN_CALL: Gas = Gas::from_tgas(250);

// Register used to receive data id from `promise_await_data`.
const DATA_ID_REGISTER: u64 = 0;

// Prepaid gas for a `clear_state_on_finish` call
const CLEAR_STATE_ON_FINISH_CALL_GAS: Gas = Gas::from_tgas(5);

// Prepaid gas for a `return_signature_on_finish` call
const RETURN_SIGNATURE_ON_FINISH_CALL_GAS: Gas = Gas::from_tgas(5);

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug)]
pub struct InitializingContractState {
    pub candidates: Candidates,
    pub threshold: usize,
    pub pk_votes: PkVotes,
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug)]
pub struct RunningContractState {
    pub epoch: u64,
    pub participants: Participants,
    pub threshold: usize,
    pub public_key: PublicKey,
    pub candidates: Candidates,
    pub join_votes: Votes,
    pub leave_votes: Votes,
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug)]
pub struct ResharingContractState {
    pub old_epoch: u64,
    pub old_participants: Participants,
    // TODO: only store diff to save on storage
    pub new_participants: Participants,
    pub threshold: usize,
    pub public_key: PublicKey,
    pub finished_votes: HashSet<AccountId>,
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug)]
pub enum ProtocolContractState {
    NotInitialized,
    Initializing(InitializingContractState),
    Running(RunningContractState),
    Resharing(ResharingContractState),
}

#[derive(BorshSerialize, BorshDeserialize, BorshStorageKey, Hash, Clone, Debug, PartialEq, Eq)]
#[borsh(crate = "near_sdk::borsh")]
pub enum StorageKey {
    PendingRequests,
}

#[near_bindgen]
#[derive(BorshDeserialize, BorshSerialize, Debug)]
pub enum VersionedMpcContract {
    V0(MpcContract),
}

impl Default for VersionedMpcContract {
    fn default() -> Self {
        env::panic_str("Calling default not allowed.");
    }
}

/// The index into calling the YieldResume feature of NEAR. This will allow to resume
/// a yield call after the contract has been called back via this index.
#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug, Clone)]
#[borsh(crate = "near_sdk::borsh")]
pub struct YieldIndex {
    data_id: CryptoHash,
}

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("Too many pending requests. Please, try again later.")]
    RequestLimitExceeded,
    #[error("Payload for this request is already requested.")]
    PayloadCollision,
    #[error("Attached deposit is insufficent: {0}.")]
    InsufficientDeposit(String),
    #[error("Signature request has timed out.")]
    Timeout,
    #[error("This sign request was removed from pending requests: timed out or completed.")]
    RequestNotInPending,
    #[error("No promise yield has been created for this request.")]
    RequestNotInYieldResume,
    #[error("Signature could not be verified.")]
    SignatureNotVerified,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolStateError {
    #[error("Protocol state is not expected: {0}.")]
    NotExpectedState(String),
}

#[derive(Debug, thiserror::Error)]
pub enum VoteError {
    #[error("Voting account is not not in the participant set.")]
    VoterNotParticipant,
    #[error("Account to be kicked is not not in the participant set.")]
    KickNotParticipant,
    #[error("Account to join is not not in the candidates set.")]
    JoinNotCandidate,
    #[error("Account to join is already in the participant set.")]
    JoinAlreadyParticipant,
    #[error("Mismatched epoch.")]
    MismatchedEpoch,
    #[error("Number of participants cannot go below threshold.")]
    ParticipantsBelowThreshold,
}

#[derive(Debug, thiserror::Error)]
pub enum MpcContractError {
    #[error("signature tx error: {0}")]
    SignatureError(SignatureError),
    #[error("Protocol state error: {0}")]
    ProtocolStateError(ProtocolStateError),
    #[error("vote error: {0}")]
    VoteError(VoteError),
}

impl FunctionError for MpcContractError {
    fn panic(&self) -> ! {
        crate::env::panic_str(&self.to_string())
    }
}

#[derive(BorshDeserialize, BorshSerialize, Serialize, Deserialize, Debug, Clone)]
#[borsh(crate = "near_sdk::borsh")]
pub struct SignatureRequest {
    pub epsilon: SerializableScalar,
    pub payload_hash: SerializableScalar,
}

impl SignatureRequest {
    pub fn new(payload_hash: Scalar, predecessor_id: &AccountId, path: &str) -> Self {
        let epsilon = derive_epsilon(predecessor_id, path);
        let epsilon = SerializableScalar { scalar: epsilon };
        let payload_hash = SerializableScalar {
            scalar: payload_hash,
        };
        SignatureRequest {
            epsilon,
            payload_hash,
        }
    }
}

#[derive(BorshDeserialize, BorshSerialize, Debug)]
pub struct MpcContract {
    protocol_state: ProtocolContractState,
    pending_requests: LookupMap<SignatureRequest, YieldIndex>,
    request_counter: u32,
}

impl MpcContract {
    fn add_request(&mut self, request: &SignatureRequest, data_id: CryptoHash) {
        if self
            .pending_requests
            .insert(request, &YieldIndex { data_id })
            .is_none()
        {
            self.request_counter += 1;
        }
    }

    fn remove_request(&mut self, request: SignatureRequest) {
        if self.pending_requests.remove(&request).is_some() {
            self.request_counter -= 1;
            Ok(())
        } else {
            Err(MpcContractError::SignatureError(
                SignatureError::RequestNotInYieldResume,
            ))
        }
    }

    pub fn init(threshold: usize, candidates: BTreeMap<AccountId, CandidateInfo>) -> Self {
        MpcContract {
            protocol_state: ProtocolContractState::Initializing(InitializingContractState {
                candidates: Candidates { candidates },
                threshold,
                pk_votes: PkVotes::new(),
            }),
            pending_requests: LookupMap::new(StorageKey::PendingRequests),
            request_counter: 0,
        }
    }
}

// User contract API
#[near_bindgen]
impl VersionedMpcContract {
    #[allow(unused_variables)]
    /// `key_version` must be less than or equal to the value at `latest_key_version`
    /// To avoid overloading the network with too many requests,
    /// we ask for a small deposit for each signature request.
    /// The fee changes based on how busy the network is.
    #[payable]
    #[handle_result]
    pub fn sign(&mut self, request: SignRequest) -> Result<(), MpcContractError> {
        let SignRequest {
            payload,
            path,
            key_version,
        } = request;
        let latest_key_version: u32 = self.latest_key_version();
        // It's important we fail here because the MPC nodes will fail in an identical way.
        // This allows users to get the error message
        let payload = Scalar::from_bytes(payload).expect("Payload hash is bad");
        assert!(
            key_version <= latest_key_version,
            "This version of the signer contract doesn't support versions greater than {}",
            latest_key_version,
        );
        // Check deposit
        let deposit = env::attached_deposit();
        let required_deposit = self.signature_deposit();
        if deposit.as_yoctonear() < required_deposit {
            return Err(MpcContractError::SignatureError(
                SignatureError::InsufficientDeposit(format!(
                    "Attached deposit is {}, required deposit is {}",
                    deposit, required_deposit
                )),
            ));
        }
        // Make sure sign call will not run out of gas doing recursive calls because the payload will never be removed
        assert!(
            env::prepaid_gas() >= GAS_FOR_SIGN_CALL,
            "Insufficient gas provided. Provided: {} Required: {}",
            env::prepaid_gas(),
            GAS_FOR_SIGN_CALL
        );

        match self {
            Self::V0(mpc_contract) => {
                if mpc_contract.request_counter > 8 {
                    env::panic_str("Too many pending requests. Please, try again later.");
                }
            }
        }
        let predecessor = env::predecessor_account_id();
        let request = SignatureRequest::new(payload, &predecessor, &path);
        if !self.request_already_exists(&request) {
            match self {
                Self::V0(mpc_contract) => {
                    let yield_promise = env::promise_yield_create(
                        "clear_state_on_finish",
                        &serde_json::to_vec(&(&request,)).unwrap(),
                        CLEAR_STATE_ON_FINISH_CALL_GAS,
                        GasWeight(0),
                        DATA_ID_REGISTER,
                    );

                    // Store the request in the contract's local state
                    let data_id: CryptoHash = env::read_register(DATA_ID_REGISTER)
                        .expect("read_register failed")
                        .try_into()
                        .expect("conversion to CryptoHash failed");

                    mpc_contract.add_request(&request, data_id);

                    log!(
                        "sign: predecessor={predecessor}, payload={payload:?}, path={path:?}, key_version={key_version}, data_id={data_id:?}",
                    );

                    env::log_str(
                        &serde_json::to_string(&near_sdk::env::random_seed_array()).unwrap(),
                    );

                    // NOTE: there's another promise after the clear_state_on_finish to avoid any errors
                    // that would rollback the state.
                    let final_yield_promise = env::promise_then(
                        yield_promise,
                        env::current_account_id(),
                        "return_signature_on_finish",
                        &[],
                        NearToken::from_near(0),
                        RETURN_SIGNATURE_ON_FINISH_CALL_GAS,
                    );
                    // The return value for this function call will be the value
                    // returned by the `sign_on_finish` callback.
                    Ok(env::promise_return(final_yield_promise))
                }
            }
        } else {
            Err(MpcContractError::SignatureError(
                SignatureError::PayloadCollision,
            ))
        }
    }

    #[private]
    #[handle_result]
    pub fn return_signature_on_finish(
        &mut self,
        #[callback_unwrap] signature: SignatureResult<SignatureResponse, SignaturePromiseError>,
    ) -> Result<SignatureResponse, MpcContractError> {
        match self {
            Self::V0(_) => match signature {
                SignatureResult::Ok(signature) => Ok(signature),
                SignatureResult::Err(_) => {
                    Err(MpcContractError::SignatureError(SignatureError::Timeout))
                }
            },
        }
    }

    #[private]
    #[handle_result]
    pub fn clear_state_on_finish(
        &mut self,
        request: SignatureRequest,
        #[callback_result] signature: Result<SignatureResponse, PromiseError>,
    ) -> Result<SignatureResult<SignatureResponse, SignaturePromiseError>, MpcContractError> {
        match self {
            Self::V0(mpc_contract) => {
                // Clean up the local state
                mpc_contract.remove_request(request)?;
                match signature {
                    Ok(signature) => Ok(SignatureResult::Ok(signature)),
                    Err(_) => Ok(SignatureResult::Err(SignaturePromiseError::Failed)),
                }
            }
        }
    }

    #[handle_result]
    pub fn respond(
        &mut self,
        request: SignatureRequest,
        response: SignatureResponse,
    ) -> Result<(), MpcContractError> {
        let protocol_state = self.mutable_state();

        if let ProtocolContractState::Running(_) = protocol_state {
            let signer = env::signer_account_id();
            log!(
                "respond: signer={}, request={:?} big_r={:?} s={:?}",
                &signer,
                &request,
                &response.big_r,
                &response.s
            );

            // generate the expected public key
            let expected_public_key = derive_key(
                near_public_key_to_affine_point(self.public_key()?),
                request.epsilon.scalar,
            );

            // Check the signature is correct
            if check_ec_signature(
                &expected_public_key,
                &response.big_r.affine_point,
                &response.s.scalar,
                request.payload_hash.scalar,
                response.recovery_id,
            )
            .is_err()
            {
                return Err(MpcContractError::SignatureError(
                    SignatureError::SignatureNotVerified,
                ));
            }

            match self {
                Self::V0(mpc_contract) => {
                    if let Some(YieldIndex { data_id }) =
                        mpc_contract.pending_requests.get(&request)
                    {
                        env::promise_yield_resume(
                            &data_id,
                            &serde_json::to_vec(&response).unwrap(),
                        );
                        Ok(())
                    } else {
                        Err(MpcContractError::SignatureError(
                            SignatureError::RequestNotInPending,
                        ))
                    }
                }
            }
        } else {
            Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("running".to_string()),
            ))
        }
    }

    /// This is the root public key combined from all the public keys of the participants.
    #[handle_result]
    pub fn public_key(&self) -> Result<PublicKey, MpcContractError> {
        match self.state() {
            ProtocolContractState::Running(state) => Ok(state.public_key.clone()),
            ProtocolContractState::Resharing(state) => Ok(state.public_key.clone()),
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("running or resharing".to_string()),
            )),
        }
    }

    /// This is the derived public key of the caller given path and predecessor
    /// if predecessor is not provided, it will be the caller of the contract
    pub fn derived_public_key(&self, path: String, predecessor: Option<AccountId>) -> PublicKey {
        let predecessor = predecessor.unwrap_or_else(env::predecessor_account_id);
        let epsilon = derive_epsilon(&predecessor, &path);
        let derived_public_key =
            derive_key(near_public_key_to_affine_point(self.public_key()), epsilon);
        let encoded_point = derived_public_key.to_encoded_point(false);
        let slice: &[u8] = &encoded_point.as_bytes()[1..65];
        let mut data: Vec<u8> = vec![near_sdk::CurveType::SECP256K1 as u8];
        data.extend(slice.to_vec());
        PublicKey::try_from(data).unwrap()
    }

    /// Key versions refer new versions of the root key that we may choose to generate on cohort changes
    /// Older key versions will always work but newer key versions were never held by older signers
    /// Newer key versions may also add new security features, like only existing within a secure enclave
    /// Currently only 0 is a valid key version
    pub const fn latest_key_version(&self) -> u32 {
        0
    }

    // contract version
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    #[handle_result]
    pub fn join(
        &mut self,
        url: String,
        sign_pk: PublicKey,
    ) -> Result<(), MpcContractError> {
        log!(
            "join: signer={}, url={}, cipher_pk={:?}, sign_pk={:?}",
            env::signer_account_id(),
            url,
            cipher_pk,
            sign_pk
        );
        let protocol_state = self.mutable_state();
        match protocol_state {
            ProtocolContractState::Running(RunningContractState {
                participants,
                ref mut candidates,
                ..
            }) => {
                let signer_account_id = env::signer_account_id();
                if participants.contains_key(&signer_account_id) {
                    return Err(MpcContractError::VoteError(
                        VoteError::JoinAlreadyParticipant,
                    ));
                }
                candidates.insert(
                    signer_account_id.clone(),
                    CandidateInfo {
                        account_id: signer_account_id,
                        url,
                        cipher_pk,
                        sign_pk,
                    },
                );
                Ok(())
            }
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("running".to_string()),
            )),
        }
    }

    #[handle_result]
    pub fn vote_join(&mut self, candidate_account_id: AccountId) -> Result<bool, MpcContractError> {
        log!(
            "vote_join: signer={}, candidate_account_id={}",
            env::signer_account_id(),
            candidate_account_id
        );
        let protocol_state = self.mutable_state();
        match protocol_state {
            ProtocolContractState::Running(RunningContractState {
                epoch,
                participants,
                threshold,
                public_key,
                candidates,
                join_votes,
                ..
            }) => {
                let signer_account_id = env::signer_account_id();
                if !participants.contains_key(&signer_account_id) {
                    return Err(MpcContractError::VoteError(VoteError::VoterNotParticipant));
                }
                let candidate_info = candidates
                    .get(&candidate_account_id)
                    .ok_or(MpcContractError::VoteError(VoteError::JoinNotCandidate))?;
                let voted = join_votes.entry(candidate_account_id.clone());
                voted.insert(signer_account_id);
                if voted.len() >= *threshold {
                    let mut new_participants = participants.clone();
                    new_participants
                        .insert(candidate_account_id.clone(), candidate_info.clone().into());
                    *protocol_state = ProtocolContractState::Resharing(ResharingContractState {
                        old_epoch: *epoch,
                        old_participants: participants.clone(),
                        new_participants,
                        threshold: *threshold,
                        public_key: public_key.clone(),
                        finished_votes: HashSet::new(),
                    });
                    Ok(true)
                } else {
            }
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("running".to_string()),
            )),
        }
    }
    }

    #[handle_result]
    pub fn vote_leave(&mut self, kick: AccountId) -> Result<bool, MpcContractError> {
        log!(
            "vote_leave: signer={}, kick={}",
            env::signer_account_id(),
            kick
        );
        let protocol_state = self.mutable_state();
        match protocol_state {
            ProtocolContractState::Running(RunningContractState {
                epoch,
                participants,
                threshold,
                public_key,
                leave_votes,
                ..
            }) => {
                let signer_account_id = env::signer_account_id();
                if !participants.contains_key(&signer_account_id) {
                    return Err(MpcContractError::VoteError(VoteError::VoterNotParticipant));
                }
                if !participants.contains_key(&kick) {
                    return Err(MpcContractError::VoteError(VoteError::KickNotParticipant));
                }
                if participants.len() <= *threshold {
                    return Err(MpcContractError::VoteError(
                        VoteError::ParticipantsBelowThreshold,
                    ));
                }
                let voted = leave_votes.entry(kick.clone());
                voted.insert(signer_account_id);
                if voted.len() >= *threshold {
                    let mut new_participants = participants.clone();
                    new_participants.remove(&kick);
                    *protocol_state = ProtocolContractState::Resharing(ResharingContractState {
                        old_epoch: *epoch,
                        old_participants: participants.clone(),
                        new_participants,
                        threshold: *threshold,
                        public_key: public_key.clone(),
                        finished_votes: HashSet::new(),
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("running".to_string()),
            )),
        }
    }

    #[handle_result]
    pub fn vote_pk(&mut self, public_key: PublicKey) -> Result<bool, MpcContractError> {
        log!(
            "vote_pk: signer={}, public_key={:?}",
            env::signer_account_id(),
            public_key
        );
        let protocol_state = self.mutable_state();
        match protocol_state {
            ProtocolContractState::Initializing(InitializingContractState {
                candidates,
                threshold,
                pk_votes,
            }) => {
                let signer_account_id = env::signer_account_id();
                if !candidates.contains_key(&signer_account_id) {
                    return Err(MpcContractError::VoteError(VoteError::VoterNotParticipant));
                }
                let voted = pk_votes.entry(public_key.clone());
                voted.insert(signer_account_id);
                if voted.len() >= *threshold {
                    *protocol_state = ProtocolContractState::Running(RunningContractState {
                        epoch: 0,
                        participants: candidates.clone().into(),
                        threshold: *threshold,
                        public_key,
                        candidates: Candidates::new(),
                        join_votes: Votes::new(),
                        leave_votes: Votes::new(),
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            ProtocolContractState::Running(state) if state.public_key == public_key => Ok(true),
            ProtocolContractState::Resharing(state) if state.public_key == public_key => Ok(true),
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState(
                    "initializing or running/resharing with the same public key".to_string(),
                ),
            )),
        }
    }

    #[handle_result]
    pub fn vote_reshared(&mut self, epoch: u64) -> Result<bool, MpcContractError> {
        log!(
            "vote_reshared: signer={}, epoch={}",
            env::signer_account_id(),
            epoch
        );
        let protocol_state = self.mutable_state();
        match protocol_state {
            ProtocolContractState::Resharing(ResharingContractState {
                old_epoch,
                old_participants,
                new_participants,
                threshold,
                public_key,
                finished_votes,
            }) => {
                if *old_epoch + 1 != epoch {
                    return Err(MpcContractError::VoteError(VoteError::MismatchedEpoch));
                }
                let signer_account_id = env::signer_account_id();
                if !old_participants.contains_key(&signer_account_id) {
                    return Err(MpcContractError::VoteError(VoteError::VoterNotParticipant));
                }
                finished_votes.insert(signer_account_id);
                if finished_votes.len() >= *threshold {
                    *protocol_state = ProtocolContractState::Running(RunningContractState {
                        epoch: *old_epoch + 1,
                        participants: new_participants.clone(),
                        threshold: *threshold,
                        public_key: public_key.clone(),
                        candidates: Candidates::new(),
                        join_votes: Votes::new(),
                        leave_votes: Votes::new(),
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            ProtocolContractState::Running(state) => {
                if state.epoch == epoch {
                    Ok(true)
                } else {
                    Err(MpcContractError::ProtocolStateError(
                        ProtocolStateError::NotExpectedState("resharing".to_string()),
                    ))
                }
            }
            _ => Err(MpcContractError::ProtocolStateError(
                ProtocolStateError::NotExpectedState("resharing".to_string()),
            )),
        }
    }
}

// Contract developer helper API
#[near_bindgen]
impl VersionedMpcContract {
    #[init]
    pub fn init(threshold: usize, candidates: BTreeMap<AccountId, CandidateInfo>) -> Self {
        log!(
            "init: signer={}, threshold={}, candidates={}",
            env::signer_account_id(),
            threshold,
            serde_json::to_string(&candidates).unwrap()
        );

        if threshold > candidates.len() {
            env::panic_str("threshold cannot be greater than the number of candidates");
        }

        Self::V0(MpcContract::init(threshold, candidates))
    }

    // This function can be used to transfer the MPC network to a new contract.
    #[private]
    #[init(ignore_state)]
    pub fn init_running(
        epoch: u64,
        participants: BTreeMap<AccountId, ParticipantInfo>,
        threshold: usize,
        public_key: PublicKey,
    ) -> Self {
        log!(
            "init_running: signer={}, epoch={}, participants={}, threshold={}, public_key={:?}",
            env::signer_account_id(),
            epoch,
            serde_json::to_string(&participants).unwrap(),
            threshold,
            public_key
        );

        if threshold > participants.len() {
            env::panic_str("threshold cannot be greater than the number of participants");
        }

        Self::V0(MpcContract {
            protocol_state: ProtocolContractState::Running(RunningContractState {
                epoch,
                participants: Participants { participants },
                threshold,
                public_key,
                candidates: Candidates::new(),
                join_votes: Votes::new(),
                leave_votes: Votes::new(),
            }),
            pending_requests: LookupMap::new(StorageKey::PendingRequests),
            request_counter: 0,
        })
    }

    pub fn state(&self) -> &ProtocolContractState {
        match self {
            Self::V0(mpc_contract) => &mpc_contract.protocol_state,
        }
    }

    #[private]
    #[init(ignore_state)]
    pub fn clean(keys: Vec<near_sdk::json_types::Base64VecU8>) -> Self {
        log!("clean: keys={:?}", keys);
        for key in keys.iter() {
            env::storage_remove(&key.0);
        }
        Self::V0(MpcContract {
            protocol_state: ProtocolContractState::NotInitialized,
            pending_requests: LookupMap::new(StorageKey::PendingRequests),
            request_counter: 0,
        })
    }

    fn mutable_state(&mut self) -> &mut ProtocolContractState {
        match self {
            Self::V0(ref mut mpc_contract) => &mut mpc_contract.protocol_state,
        }
    }

    fn request_already_exists(&self, request: &SignatureRequest) -> bool {
        match self {
            Self::V0(mpc_contract) => mpc_contract.pending_requests.contains_key(request),
        }
    }

    fn signature_deposit(&self) -> u128 {
        const CHEAP_REQUESTS: u32 = 3;
        let pending_requests = match self {
            Self::V0(mpc_contract) => mpc_contract.request_counter,
        };
        match pending_requests {
            0..=CHEAP_REQUESTS => 1,
            _ => {
                (pending_requests - CHEAP_REQUESTS) as u128
                    * NearToken::from_millinear(50).as_yoctonear()
            }
        }
    }
}
