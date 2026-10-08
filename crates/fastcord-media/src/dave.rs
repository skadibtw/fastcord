//! DAVE v1 MLS and frame-encryption adapter (SPEC §6.4).
//!
//! This is the only boundary to `davey`: protocol state, transition epochs,
//! and readiness are owned here so the voice driver cannot accidentally send
//! or deliver transport-only media.

use openmls::prelude::{ProposalOrRefType, ProposalType, Sender};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU16;

use davey::{CommitWelcome, DaveSession as DaveEngine, MediaType, ProposalsOperationType};
use tls_codec::Serialize;

const DAVE_PROTOCOL_VERSION: u16 = 1;
const OPUS_SILENCE: &[u8] = &[0xF8, 0xFF, 0xFE];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaveError {
    UnsupportedProtocolVersion,
    InvalidInput,
    Engine,
    UnexpectedEpoch,
    UnexpectedTransition,
    NotReady,
    NotMember,
    PlaintextBypass,
}

impl fmt::Display for DaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedProtocolVersion => "unsupported DAVE protocol version",
            Self::InvalidInput => "invalid DAVE message",
            Self::Engine => "DAVE operation failed",
            Self::UnexpectedEpoch => "unexpected DAVE epoch",
            Self::UnexpectedTransition => "unexpected DAVE transition",
            Self::NotReady => "DAVE media keys are not ready",
            Self::NotMember => "local user is not in the DAVE group",
            Self::PlaintextBypass => "DAVE refused an unencrypted media frame",
        })
    }
}

impl std::error::Error for DaveError {}

#[derive(Debug, Clone, Copy)]
struct Transition {
    id: u16,
    ready: bool,
}

/// MLS state for one Discord media session. It deliberately has no passthrough
/// API: every accepted media frame is encrypted or authenticated by DAVE.
pub(crate) struct DaveSession {
    engine: DaveEngine,
    user_id: u64,
    channel_id: u64,
    protocol_version: NonZeroU16,
    external_sender: Option<Vec<u8>>,
    announced_epoch: Option<u64>,
    transition: Option<Transition>,
    in_group: bool,
    send_enabled: bool,
    cached_proposals: Vec<Vec<u8>>,
}

impl fmt::Debug for DaveSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaveSession")
            .field("protocol_version", &self.protocol_version)
            .field("user_id", &self.user_id)
            .field("channel_id", &self.channel_id)
            .field("epoch", &self.epoch())
            .field("ready", &self.is_send_ready())
            .finish()
    }
}

impl DaveSession {
    pub(crate) fn new(
        protocol_version: u16,
        user_id: u64,
        channel_id: u64,
    ) -> Result<Self, DaveError> {
        let protocol_version = NonZeroU16::new(protocol_version)
            .filter(|version| version.get() == DAVE_PROTOCOL_VERSION)
            .ok_or(DaveError::UnsupportedProtocolVersion)?;
        let engine = DaveEngine::new(protocol_version, user_id, channel_id, None)
            .map_err(|_| DaveError::Engine)?;
        Ok(Self {
            engine,
            user_id,
            channel_id,
            protocol_version,
            external_sender: None,
            announced_epoch: None,
            transition: None,
            in_group: false,
            send_enabled: false,
            cached_proposals: Vec::new(),
        })
    }

    pub(crate) fn create_key_package(&mut self) -> Result<Vec<u8>, DaveError> {
        self.engine
            .create_key_package()
            .map_err(|_| DaveError::Engine)
    }

    pub(crate) fn set_external_sender(&mut self, sender: &[u8]) -> Result<(), DaveError> {
        if sender.is_empty() {
            return Err(DaveError::InvalidInput);
        }
        if self.external_sender.as_deref() == Some(sender) {
            return Ok(());
        }
        if self.external_sender.is_some() && self.engine.is_ready() {
            return Err(DaveError::InvalidInput);
        }
        self.engine
            .set_external_sender(sender)
            .map_err(|_| DaveError::Engine)?;
        self.external_sender = Some(sender.to_vec());
        Ok(())
    }

    pub(crate) fn process_proposals(
        &mut self,
        operation: u8,
        proposals: &[u8],
        expected_user_ids: &[u64],
    ) -> Result<Option<CommitWelcome>, DaveError> {
        let operation = match operation {
            0 => ProposalsOperationType::APPEND,
            1 => ProposalsOperationType::REVOKE,
            _ => return Err(DaveError::InvalidInput),
        };
        let result = self
            .engine
            .process_proposals(operation, proposals, Some(expected_user_ids))
            .map_err(|_| DaveError::Engine)?;
        self.refresh_cached_proposals()?;
        Ok(result)
    }

    /// Starts or recreates the announced MLS epoch. Epoch one is the DAVE
    /// protocol's explicit group-reset signal and requires a fresh key package.
    pub(crate) fn prepare_epoch(&mut self, epoch: u64) -> Result<Option<Vec<u8>>, DaveError> {
        if epoch == 0 {
            return Err(DaveError::UnexpectedEpoch);
        }
        if epoch != 1
            && self.epoch().is_some_and(|current| {
                epoch < current || current.checked_add(1).is_some_and(|next| epoch > next)
            })
        {
            return Err(DaveError::UnexpectedEpoch);
        }
        self.send_enabled = false;
        self.transition = None;
        if epoch == 1 {
            self.in_group = false;
            self.cached_proposals.clear();
            self.announced_epoch = Some(epoch);
            self.engine
                .reinit(self.protocol_version, self.user_id, self.channel_id, None)
                .map_err(|_| DaveError::Engine)?;
            Ok(Some(self.create_key_package()?))
        } else {
            self.announced_epoch = Some(epoch);
            Ok(None)
        }
    }

    /// Returns whether opcode 23 must be sent now.
    pub(crate) fn prepare_transition(
        &mut self,
        transition_id: u16,
        protocol_version: u16,
    ) -> Result<bool, DaveError> {
        if protocol_version != DAVE_PROTOCOL_VERSION {
            return Err(DaveError::UnsupportedProtocolVersion);
        }
        if self.announced_epoch == self.epoch() {
            self.announced_epoch = None;
        }
        let ready = transition_id != 0 && self.announced_epoch.is_none() && self.group_is_ready();
        self.transition = Some(Transition {
            id: transition_id,
            ready,
        });
        Ok(ready)
    }

    /// Processes an announced commit. Callers report failures with opcode 31
    /// and then invoke `recover` before accepting any more media.
    pub(crate) fn process_commit(
        &mut self,
        transition_id: u16,
        commit: &[u8],
    ) -> Result<(), DaveError> {
        let references = commit_proposal_references(commit).ok_or(DaveError::InvalidInput)?;
        let unique_references: HashSet<_> = references.iter().collect();
        if references.len() != self.cached_proposals.len()
            || unique_references.len() != references.len()
            || references
                .iter()
                .any(|reference| !self.cached_proposals.contains(reference))
        {
            return Err(DaveError::InvalidInput);
        }
        let expected_epoch = self.expected_next_epoch();
        self.send_enabled = false;
        self.engine
            .process_commit(commit)
            .map_err(|_| DaveError::Engine)?;
        self.cached_proposals.clear();
        self.confirm_epoch(expected_epoch)?;
        self.in_group = self.local_member_present();
        self.transition = Some(Transition {
            id: transition_id,
            ready: self.group_is_ready(),
        });
        if !self.group_is_ready() {
            return Err(DaveError::NotMember);
        }
        Ok(())
    }

    /// Processes the Welcome addressed to this client. The Welcome is the
    /// authenticated group installation point for a newly joined member.
    pub(crate) fn process_welcome(
        &mut self,
        transition_id: u16,
        welcome: &[u8],
    ) -> Result<(), DaveError> {
        self.send_enabled = false;
        self.engine
            .process_welcome(welcome)
            .map_err(|_| DaveError::Engine)?;
        self.confirm_epoch(self.announced_epoch)?;
        self.cached_proposals.clear();
        self.in_group = self.local_member_present();
        self.transition = Some(Transition {
            id: transition_id,
            ready: self.group_is_ready(),
        });
        if !self.group_is_ready() {
            return Err(DaveError::NotMember);
        }
        Ok(())
    }

    pub(crate) fn execute_transition(&mut self, transition_id: u16) -> Result<(), DaveError> {
        let Some(transition) = self.transition.take() else {
            return Err(DaveError::UnexpectedTransition);
        };
        if transition.id != transition_id {
            self.transition = Some(transition);
            return Err(DaveError::UnexpectedTransition);
        }
        if transition.ready && self.group_is_ready() {
            self.send_enabled = true;
        }
        // ID zero is the specified immediate reinitialization transition. It
        // may execute while a fresh group is still pending, but never enables
        // media until a later authenticated Commit or Welcome installs keys.
        Ok(())
    }

    pub(crate) fn recover(&mut self, transition_id: u16) -> Result<Vec<u8>, DaveError> {
        self.send_enabled = false;
        self.engine
            .reinit(self.protocol_version, self.user_id, self.channel_id, None)
            .map_err(|_| DaveError::Engine)?;
        self.in_group = false;
        self.cached_proposals.clear();
        self.announced_epoch = None;
        self.transition = Some(Transition {
            id: transition_id,
            ready: false,
        });
        self.create_key_package()
    }

    pub(crate) fn is_send_ready(&self) -> bool {
        self.send_enabled && self.group_is_ready()
    }

    pub(crate) fn is_receive_ready(&self) -> bool {
        self.group_is_ready()
    }

    pub(crate) fn transition_id(&self) -> Option<u16> {
        self.transition.map(|transition| transition.id)
    }

    pub(crate) fn epoch(&self) -> Option<u64> {
        self.engine.epoch().map(|epoch| epoch.as_u64())
    }

    pub(crate) fn encrypt_opus<'a>(
        &mut self,
        packet: &'a [u8],
    ) -> Result<Cow<'a, [u8]>, DaveError> {
        if !self.is_send_ready() {
            return Err(DaveError::NotReady);
        }
        if packet == OPUS_SILENCE {
            return Err(DaveError::PlaintextBypass);
        }
        let encrypted = self
            .engine
            .encrypt_opus(packet)
            .map_err(|_| DaveError::Engine)?;
        if matches!(encrypted, Cow::Borrowed(_)) || encrypted.len() <= packet.len() {
            return Err(DaveError::PlaintextBypass);
        }
        Ok(encrypted)
    }

    pub(crate) fn decrypt_opus(
        &mut self,
        user_id: u64,
        packet: &[u8],
    ) -> Result<Vec<u8>, DaveError> {
        if !self.is_receive_ready() {
            return Err(DaveError::NotReady);
        }
        if packet == OPUS_SILENCE {
            return Err(DaveError::PlaintextBypass);
        }
        let frame = self
            .engine
            .decrypt(user_id, MediaType::AUDIO, packet)
            .map_err(|_| DaveError::Engine)?;
        if frame == packet {
            return Err(DaveError::PlaintextBypass);
        }
        Ok(frame)
    }

    fn group_is_ready(&self) -> bool {
        self.engine.is_ready() && self.in_group
    }

    fn local_member_present(&self) -> bool {
        self.engine
            .get_user_ids()
            .is_some_and(|members| members.contains(&self.user_id))
    }

    fn expected_next_epoch(&self) -> Option<u64> {
        self.announced_epoch
            .or_else(|| self.epoch().and_then(|epoch| epoch.checked_add(1)))
    }

    fn confirm_epoch(&mut self, expected: Option<u64>) -> Result<(), DaveError> {
        let actual = self.epoch().ok_or(DaveError::UnexpectedEpoch)?;
        if expected.is_some_and(|expected| expected != actual) {
            return Err(DaveError::UnexpectedEpoch);
        }
        self.announced_epoch = None;
        Ok(())
    }

    fn refresh_cached_proposals(&mut self) -> Result<(), DaveError> {
        let Some(group) = self.engine.group() else {
            self.cached_proposals.clear();
            return Ok(());
        };
        let proposals = group
            .pending_proposals()
            .map(|proposal| {
                if proposal.proposal_or_ref_type() != ProposalOrRefType::Reference
                    || !matches!(
                        proposal.proposal().proposal_type(),
                        ProposalType::Add | ProposalType::Remove
                    )
                    || !matches!(proposal.sender(), Sender::External(_))
                {
                    return Err(DaveError::InvalidInput);
                }
                proposal
                    .proposal_reference_ref()
                    .tls_serialize_detached()
                    .map_err(|_| DaveError::Engine)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.cached_proposals = proposals;
        Ok(())
    }
}

/// DAVE accepts only public MLS commits whose proposal vector consists of
/// proposal references. Parsing is deliberately read-only and precedes any
/// state mutation by `davey`.
fn commit_proposal_references(message: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut cursor = 0;
    if read_u16(message, &mut cursor) != Some(1) || read_u16(message, &mut cursor) != Some(1) {
        return None;
    }
    take_quic_vector(message, &mut cursor)?;
    take(message, &mut cursor, 8)?;
    match take(message, &mut cursor, 1)?.first().copied()? {
        1 | 2 => {
            take(message, &mut cursor, 4)?;
        }
        3 | 4 => {}
        _ => return None,
    }
    take_quic_vector(message, &mut cursor)?;
    if take(message, &mut cursor, 1)? != [3] {
        return None;
    }
    let proposals = take_quic_vector(message, &mut cursor)?;
    if proposals.is_empty() {
        return None;
    }
    let mut proposal_cursor = 0;
    let mut references = Vec::new();
    while proposal_cursor < proposals.len() {
        if take(proposals, &mut proposal_cursor, 1)? != [2] {
            return None;
        }
        let reference_start = proposal_cursor;
        let reference = take_quic_vector(proposals, &mut proposal_cursor)?;
        if reference.is_empty() {
            return None;
        }
        references.push(proposals[reference_start..proposal_cursor].to_vec());
    }
    (proposal_cursor == proposals.len()).then_some(references)
}

#[cfg(test)]
fn commit_has_only_reference_proposals(message: &[u8]) -> bool {
    commit_proposal_references(message).is_some()
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
    let bytes = take(bytes, cursor, 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let result = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(result)
}

fn take_quic_vector<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let first = *take(bytes, cursor, 1)?.first()?;
    let prefix_len = 1_usize << (first >> 6);
    let mut len = u64::from(first & 0x3F);
    for byte in take(bytes, cursor, prefix_len.checked_sub(1)?)? {
        len = len.checked_shl(8)?.checked_add(u64::from(*byte))?;
    }
    take(bytes, cursor, usize::try_from(len).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openmls::prelude::{
        BasicCredential, Ciphersuite, CredentialWithKey, Extension, Extensions, ExternalProposal,
        ExternalSender, GroupId, KeyPackage, KeyPackageIn, MlsGroup, MlsGroupCreateConfig,
        MlsMessageBodyOut, MlsMessageOut, PURE_PLAINTEXT_WIRE_FORMAT_POLICY, SenderExtensionIndex,
    };
    use openmls::versions::ProtocolVersion;
    use openmls_basic_credential::SignatureKeyPair;
    use openmls_rust_crypto::OpenMlsRustCrypto;
    use openmls_traits::OpenMlsProvider;
    use tls_codec::{DeserializeBytes, Serialize, VLBytes};

    const SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;

    fn key_package(session: &mut DaveSession, provider: &OpenMlsRustCrypto) -> KeyPackage {
        KeyPackageIn::tls_deserialize_exact_bytes(&session.create_key_package().unwrap())
            .unwrap()
            .validate(provider.crypto(), ProtocolVersion::Mls10)
            .unwrap()
    }

    fn activate(session: &mut DaveSession, transition_id: u16) {
        assert!(
            session
                .prepare_transition(transition_id, DAVE_PROTOCOL_VERSION)
                .unwrap()
        );
        session.execute_transition(transition_id).unwrap();
        assert!(session.is_send_ready());
    }

    fn proposal_payload(proposal: &MlsMessageOut) -> Vec<u8> {
        VLBytes::new(proposal.tls_serialize_detached().unwrap())
            .tls_serialize_detached()
            .unwrap()
    }

    fn add_proposal(
        proposer: &DaveSession,
        package: KeyPackage,
        signer: &SignatureKeyPair,
    ) -> MlsMessageOut {
        let group = proposer.engine.group().unwrap();
        ExternalProposal::new_add::<OpenMlsRustCrypto>(
            package,
            group.group_id().clone(),
            group.epoch(),
            signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap()
    }

    fn remove_proposal(
        proposer: &DaveSession,
        user_id: u64,
        signer: &SignatureKeyPair,
    ) -> MlsMessageOut {
        let group = proposer.engine.group().unwrap();
        let leaf_index = group
            .members()
            .find(|member| {
                member.credential.serialized_content() == user_id.to_be_bytes().as_slice()
            })
            .unwrap()
            .index;
        ExternalProposal::new_remove::<OpenMlsRustCrypto>(
            leaf_index,
            group.group_id().clone(),
            group.epoch(),
            signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap()
    }

    fn round_trip(sender: &mut DaveSession, receiver: &mut DaveSession, sender_id: u64) {
        let clear = b"actual davey encrypted Opus frame";
        let encrypted = sender.encrypt_opus(clear).unwrap();
        assert_ne!(encrypted.as_ref(), clear);
        assert_eq!(
            receiver
                .decrypt_opus(sender_id, encrypted.as_ref())
                .unwrap(),
            clear
        );
    }

    #[test]
    fn actual_mls_welcome_rekey_membership_churn_and_opus_round_trip() {
        let channel_id = 2_u64;
        let alice_id = 1_u64;
        let bob_id = 2_u64;
        let carol_id = 3_u64;
        let mut alice = DaveSession::new(DAVE_PROTOCOL_VERSION, alice_id, channel_id).unwrap();
        let mut bob = DaveSession::new(DAVE_PROTOCOL_VERSION, bob_id, channel_id).unwrap();
        let mut carol = DaveSession::new(DAVE_PROTOCOL_VERSION, carol_id, channel_id).unwrap();
        let provider = OpenMlsRustCrypto::default();
        let delivery_signer = SignatureKeyPair::new(SUITE.signature_algorithm()).unwrap();
        delivery_signer.store(provider.storage()).unwrap();
        let external_sender = ExternalSender::new(
            delivery_signer.public().into(),
            BasicCredential::new(b"voice-gateway".to_vec()).into(),
        );
        let external_sender_bytes = external_sender.tls_serialize_detached().unwrap();
        for session in [&mut alice, &mut bob, &mut carol] {
            session.set_external_sender(&external_sender_bytes).unwrap();
        }

        let initial_add = add_proposal(&alice, key_package(&mut bob, &provider), &delivery_signer);
        let initial_payload = proposal_payload(&initial_add);
        let initial_expected = [alice_id, bob_id];
        let initial = alice
            .process_proposals(0, &initial_payload, &initial_expected)
            .unwrap()
            .unwrap();
        assert!(commit_has_only_reference_proposals(&initial.commit));
        alice.process_commit(1, &initial.commit).unwrap();
        bob.process_welcome(1, initial.welcome.as_deref().unwrap())
            .unwrap();
        activate(&mut alice, 1);
        activate(&mut bob, 1);
        round_trip(&mut alice, &mut bob, alice_id);
        round_trip(&mut bob, &mut alice, bob_id);

        let expected_users = [alice_id, bob_id, carol_id];
        let add_carol = add_proposal(&alice, key_package(&mut carol, &provider), &delivery_signer);
        let add_carol_payload = proposal_payload(&add_carol);
        let alice_add = alice
            .process_proposals(0, &add_carol_payload, &expected_users)
            .unwrap()
            .unwrap();
        let bob_add = bob
            .process_proposals(0, &add_carol_payload, &expected_users)
            .unwrap()
            .unwrap();
        assert!(commit_has_only_reference_proposals(&alice_add.commit));
        assert!(commit_has_only_reference_proposals(&bob_add.commit));

        let references = commit_proposal_references(&alice_add.commit).unwrap();
        let mut inline_commit = alice_add.commit.clone();
        let mut marker = Vec::with_capacity(references[0].len() + 1);
        marker.push(2);
        marker.extend_from_slice(&references[0]);
        let proposal_tag = inline_commit
            .windows(marker.len())
            .position(|bytes| bytes == marker)
            .unwrap();
        inline_commit[proposal_tag] = 1;
        let epoch_before_invalid_commit = alice.epoch();
        assert_eq!(
            alice.process_commit(5, &inline_commit),
            Err(DaveError::InvalidInput)
        );
        assert_eq!(alice.epoch(), epoch_before_invalid_commit);

        alice.process_commit(3, &alice_add.commit).unwrap();
        bob.process_commit(3, &alice_add.commit).unwrap();
        carol
            .process_welcome(3, alice_add.welcome.as_deref().unwrap())
            .unwrap();
        activate(&mut alice, 3);
        activate(&mut bob, 3);
        activate(&mut carol, 3);
        round_trip(&mut alice, &mut carol, alice_id);
        let retained_epoch = alice.epoch().unwrap();
        assert_ne!(retained_epoch, 1);
        assert!(alice.prepare_epoch(retained_epoch).unwrap().is_none());
        assert!(alice.prepare_transition(4, DAVE_PROTOCOL_VERSION).unwrap());
        alice.execute_transition(4).unwrap();
        assert!(alice.is_send_ready());
        assert_eq!(alice.epoch(), Some(retained_epoch));

        let remove_bob = remove_proposal(&alice, bob_id, &delivery_signer);
        let remove_bob_payload = proposal_payload(&remove_bob);
        let alice_remove = alice
            .process_proposals(0, &remove_bob_payload, &expected_users)
            .unwrap()
            .unwrap();
        let carol_remove = carol
            .process_proposals(0, &remove_bob_payload, &expected_users)
            .unwrap()
            .unwrap();
        assert!(commit_has_only_reference_proposals(&alice_remove.commit));
        assert!(commit_has_only_reference_proposals(&carol_remove.commit));
        alice.process_commit(5, &alice_remove.commit).unwrap();
        carol.process_commit(5, &alice_remove.commit).unwrap();
        if bob.process_commit(5, &alice_remove.commit).is_err() {
            bob.recover(5).unwrap();
        }
        activate(&mut alice, 5);
        activate(&mut carol, 5);
        assert_eq!(
            bob.decrypt_opus(alice_id, b"departed-member-frame"),
            Err(DaveError::NotReady)
        );
        assert!(!bob.is_receive_ready());
        assert!(!bob.is_send_ready());
        round_trip(&mut alice, &mut carol, alice_id);
    }

    #[test]
    fn reset_welcome_clears_epoch_before_the_next_commit() {
        let channel_id = 12_u64;
        let mut client = DaveSession::new(DAVE_PROTOCOL_VERSION, 1, channel_id).unwrap();
        let provider = OpenMlsRustCrypto::default();
        let delivery_signer = SignatureKeyPair::new(SUITE.signature_algorithm()).unwrap();
        delivery_signer.store(provider.storage()).unwrap();
        let external_sender = ExternalSender::new(
            delivery_signer.public().into(),
            BasicCredential::new(b"voice-gateway".to_vec()).into(),
        );
        client
            .set_external_sender(&external_sender.tls_serialize_detached().unwrap())
            .unwrap();
        let client_package = client.prepare_epoch(1).unwrap().unwrap();

        let owner_signer = SignatureKeyPair::new(SUITE.signature_algorithm()).unwrap();
        owner_signer.store(provider.storage()).unwrap();
        let owner_with_key = CredentialWithKey {
            credential: BasicCredential::new(99_u64.to_be_bytes().to_vec()).into(),
            signature_key: owner_signer.public().into(),
        };
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(SUITE)
            .with_group_context_extensions(
                Extensions::single(Extension::ExternalSenders(vec![external_sender.clone()]))
                    .unwrap(),
            )
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build();
        let mut group = MlsGroup::new_with_group_id(
            &provider,
            &owner_signer,
            &config,
            GroupId::from_slice(&channel_id.to_be_bytes()),
            owner_with_key,
        )
        .unwrap();
        let client_package = KeyPackageIn::tls_deserialize_exact_bytes(&client_package)
            .unwrap()
            .validate(provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        let (_, welcome_message, _) = group
            .add_members(&provider, &owner_signer, &[client_package])
            .unwrap();
        group.merge_pending_commit(&provider).unwrap();
        let MlsMessageBodyOut::Welcome(welcome) = welcome_message.body() else {
            panic!("expected Welcome")
        };
        let welcome = welcome.tls_serialize_detached().unwrap();
        client.process_welcome(1, &welcome).unwrap();
        assert_eq!(client.announced_epoch, None);
        activate(&mut client, 1);

        let mut carol = DaveSession::new(DAVE_PROTOCOL_VERSION, 3, channel_id).unwrap();
        carol
            .set_external_sender(&external_sender.tls_serialize_detached().unwrap())
            .unwrap();
        let proposal = add_proposal(
            &client,
            key_package(&mut carol, &provider),
            &delivery_signer,
        );
        let committed = client
            .process_proposals(0, &proposal_payload(&proposal), &[1, 3])
            .unwrap()
            .unwrap();
        client.process_commit(2, &committed.commit).unwrap();
        assert_eq!(client.epoch(), Some(2));
        activate(&mut client, 2);
    }

    #[test]
    fn media_is_gated_until_a_valid_group_transition_executes() {
        let mut session = DaveSession::new(DAVE_PROTOCOL_VERSION, 1, 2).unwrap();
        assert!(!session.is_send_ready());
        assert_eq!(
            session.encrypt_opus(b"unready").unwrap_err(),
            DaveError::NotReady
        );
        assert_eq!(
            session.prepare_transition(7, 0).unwrap_err(),
            DaveError::UnsupportedProtocolVersion
        );
        assert!(!session.is_send_ready());
    }
    #[test]
    fn key_packages_use_real_mls_crypto_and_epochs_stay_paused_until_installed() {
        let mut session = DaveSession::new(DAVE_PROTOCOL_VERSION, 1, 2).unwrap();
        let first = session.create_key_package().unwrap();
        let second = session.create_key_package().unwrap();
        assert!(!first.is_empty());
        assert_ne!(first, second);
        assert!(!session.is_receive_ready());
        assert!(!session.is_send_ready());
        assert!(session.prepare_epoch(1).unwrap().is_some());
        assert_eq!(session.epoch(), None);
        assert!(!session.is_send_ready());
        session
            .prepare_transition(0, DAVE_PROTOCOL_VERSION)
            .unwrap();
        session.execute_transition(0).unwrap();
        assert!(!session.is_send_ready());
        let recovered = session.recover(9).unwrap();
        assert!(!recovered.is_empty());
        assert!(!session.is_receive_ready());
        assert!(!session.is_send_ready());
        assert_eq!(session.transition_id(), Some(9));
    }

    #[test]
    fn media_transform_never_passes_plaintext_or_silence_through() {
        let mut session = DaveSession::new(DAVE_PROTOCOL_VERSION, 1, 2).unwrap();
        for frame in [b"pcm".as_slice(), OPUS_SILENCE] {
            assert_eq!(
                session.encrypt_opus(frame).unwrap_err(),
                DaveError::NotReady
            );
            assert_eq!(
                session.decrypt_opus(7, frame).unwrap_err(),
                DaveError::NotReady
            );
        }
    }
}
