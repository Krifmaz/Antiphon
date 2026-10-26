//! MLS (RFC 9420) groups, bound to Antiphon identities (spec §3
//! "Groups -- MLS (RFC 9420), scales cleanly to 100+ members").
//!
//! A thin wrapper over [`openmls`]: this crate does not implement MLS, it
//! decides how MLS meets the rest of Antiphon.
//!
//! # Identity binding
//!
//! An MLS member's credential is a *Basic* credential whose identity is
//! the member's Ed25519 identity key, and the leaf's signature key is that
//! same key; everything MLS signs for us is signed by
//! [`IdentityKeyPair::sign`]. So a group member is not a fresh MLS-only
//! keypair someone could vouch for separately -- it is exactly the identity
//! a 1:1 contact pins and verifies with a safety code. Every leaf this
//! wrapper lets in (a KeyPackage we add, every member of a group we join,
//! every Add in a commit we process) is checked to have
//! `credential == signature key`; a leaf that names one identity but signs
//! with another is refused. Callers then compare
//! [`GroupMember::members`] against who they expected -- the transport
//! a KeyPackage came over, the people in the call -- exactly as they would
//! for a 1:1 session.
//!
//! MLS's signatures are over `SignContent`, which starts with the
//! length-prefixed label `"MLS 1.0 ..."`. Every other use of the identity
//! key in this crate signs under its own `voip-app/...` prefix, so an MLS
//! signature can never be replayed as a prekey or route-record signature,
//! nor the reverse.
//!
//! # API for other layers
//!
//! Group calls use it today; group chats and communities are meant to
//! reuse it unchanged. Every input and output is plain bytes (the MLS
//! wire encoding), so it rides any transport:
//!
//! - [`GroupMember::new`] for an identity, then either
//!   [`create`](GroupMember::create) a group or publish a
//!   [`key_package`](GroupMember::key_package) and wait to be added.
//! - A committer calls [`add`](GroupMember::add) / [`remove`](GroupMember::remove),
//!   which apply locally at once and return the [`Commit`] to send every
//!   *other* existing member and, for adds, the Welcome for the newcomers.
//! - Everyone else feeds what arrives to [`join`](GroupMember::join)
//!   (a Welcome) or [`process`](GroupMember::process) (commits and
//!   application messages).
//! - [`encrypt`](GroupMember::encrypt) seals an application message for
//!   the whole group; [`export_secret`](GroupMember::export_secret) and
//!   [`sframe_base_key`](GroupMember::sframe_base_key) give per-epoch keys
//!   for anything else (SFrame media, [`crate::sframe`]).
//!
//! MLS needs commits applied in one order by everyone. There is no
//! delivery service here, so the *caller* must make sure only one member
//! commits at a time -- group calls pick the member at the lowest leaf
//! index (see `antiphon` `group.rs`). Two concurrent commits for the same
//! epoch would fork the group; the second to arrive is refused by
//! [`process`](GroupMember::process) with [`GroupError::Mls`], and the
//! caller has to rejoin.

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::{Signer, SignerError};
use openmls_traits::types::SignatureScheme;
use openmls_traits::OpenMlsProvider;

use crate::IdentityKeyPair;

/// X25519, AES-128-GCM, SHA-256, Ed25519 -- the RFC 9420 mandatory suite,
/// and the one whose signature scheme is our identity keys'.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Why an MLS operation failed.
#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("not in a group")]
    NoGroup,
    #[error("already in a group")]
    AlreadyInGroup,
    #[error("this MLS message is malformed or of the wrong kind")]
    Malformed,
    #[error("a member's credential does not match the key it signs with")]
    CredentialMismatch,
    #[error("that person is not in the group")]
    NotAMember,
    #[error("MLS: {0}")]
    Mls(String),
}

fn mls<E: std::fmt::Debug>(e: E) -> GroupError {
    GroupError::Mls(format!("{e:?}"))
}

/// Signs for MLS with the identity's Ed25519 key.
struct IdentitySigner(IdentityKeyPair);

impl Signer for IdentitySigner {
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        Ok(self.0.sign(payload).to_vec())
    }

    fn signature_scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// A commit we made: send `commit` to every member other than ourselves
/// and the newcomers, and `welcome` to the newcomers.
pub struct Commit {
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    /// The epoch the group is now in.
    pub epoch: u64,
}

/// What processing an incoming message produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Processed {
    /// An application message, and the identity key of whoever sent it.
    Application { sender: [u8; 32], data: Vec<u8> },
    /// A commit moved the group to `epoch`. `removed_self` means the
    /// commit removed us: the group is gone and nothing more decrypts.
    NewEpoch { epoch: u64, removed_self: bool },
    /// A proposal on its own, stored for the next commit. Group calls
    /// never send these; kept so a caller sees one rather than an error.
    Proposal,
}

/// One identity's MLS state: its key-package secrets and, once created
/// or joined, the group.
pub struct GroupMember {
    provider: OpenMlsRustCrypto,
    signer: IdentitySigner,
    identity_key: [u8; 32],
    group: Option<MlsGroup>,
}

impl GroupMember {
    pub fn new(identity: &IdentityKeyPair) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: IdentitySigner(IdentityKeyPair::from_master_seed(identity.master_seed())),
            identity_key: identity.public().identity_key,
            group: None,
        }
    }

    fn credential(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.identity_key.to_vec()).into(),
            signature_key: self.identity_key.to_vec().into(),
        }
    }

    fn create_config() -> MlsGroupCreateConfig {
        MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            // The Welcome carries the tree, so joining needs nothing else.
            .use_ratchet_tree_extension(true)
            // Handshake messages encrypted too: a relay forwarding them
            // learns nothing about who added or removed whom.
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .build()
    }

    fn join_config() -> MlsGroupJoinConfig {
        MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .build()
    }

    /// A fresh KeyPackage (MLS wire encoding) for someone to add us with.
    /// Its private half stays here, so the Welcome must come back to this
    /// same `GroupMember`.
    pub fn key_package(&mut self) -> Result<Vec<u8>, GroupError> {
        let bundle = KeyPackage::builder()
            .build(CIPHERSUITE, &self.provider, &self.signer, self.credential())
            .map_err(mls)?;
        MlsMessageOut::from(bundle.key_package().clone()).tls_serialize_detached().map_err(mls)
    }

    /// Starts a group with only ourselves in it.
    pub fn create(&mut self) -> Result<(), GroupError> {
        if self.group.is_some() {
            return Err(GroupError::AlreadyInGroup);
        }
        let group = MlsGroup::new(&self.provider, &self.signer, &Self::create_config(), self.credential())
            .map_err(mls)?;
        self.group = Some(group);
        Ok(())
    }

    fn group(&self) -> Result<&MlsGroup, GroupError> {
        self.group.as_ref().filter(|g| g.is_active()).ok_or(GroupError::NoGroup)
    }

    /// Whether we're in an active group.
    pub fn in_group(&self) -> bool {
        self.group().is_ok()
    }

    /// Leaves whatever group we were in, locally. For starting over after
    /// a fork or a missed commit.
    pub fn reset(&mut self) {
        self.group = None;
    }

    /// Checks a KeyPackage and returns the identity key it is for.
    pub fn key_package_identity(&self, key_package: &[u8]) -> Result<[u8; 32], GroupError> {
        leaf_identity(self.validate_key_package(key_package)?.leaf_node())
    }

    fn validate_key_package(&self, bytes: &[u8]) -> Result<KeyPackage, GroupError> {
        let message = MlsMessageIn::tls_deserialize_exact(bytes).map_err(|_| GroupError::Malformed)?;
        let MlsMessageBodyIn::KeyPackage(kp) = message.extract() else {
            return Err(GroupError::Malformed);
        };
        let kp = kp.validate(self.provider.crypto(), ProtocolVersion::Mls10).map_err(mls)?;
        if kp.ciphersuite() != CIPHERSUITE {
            return Err(GroupError::Malformed);
        }
        leaf_identity(kp.leaf_node())?;
        Ok(kp)
    }

    /// Adds the people these KeyPackages belong to, in one commit.
    pub fn add(&mut self, key_packages: &[&[u8]]) -> Result<Commit, GroupError> {
        let kps = key_packages.iter().map(|kp| self.validate_key_package(kp)).collect::<Result<Vec<_>, _>>()?;
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().filter(|g| g.is_active()).ok_or(GroupError::NoGroup)?;
        let (commit, welcome, _) = group.add_members(provider, signer, &kps).map_err(mls)?;
        group.merge_pending_commit(provider).map_err(mls)?;
        Ok(Commit {
            commit: commit.tls_serialize_detached().map_err(mls)?,
            welcome: Some(welcome.tls_serialize_detached().map_err(mls)?),
            epoch: group.epoch().as_u64(),
        })
    }

    /// Removes these identities, in one commit.
    pub fn remove(&mut self, identity_keys: &[[u8; 32]]) -> Result<Commit, GroupError> {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().filter(|g| g.is_active()).ok_or(GroupError::NoGroup)?;
        let leaves = identity_keys
            .iter()
            .map(|key| {
                group
                    .members()
                    .find(|m| m.signature_key == key)
                    .map(|m| m.index)
                    .ok_or(GroupError::NotAMember)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (commit, welcome, _) = group.remove_members(provider, signer, &leaves).map_err(mls)?;
        group.merge_pending_commit(provider).map_err(mls)?;
        Ok(Commit {
            commit: commit.tls_serialize_detached().map_err(mls)?,
            welcome: welcome.map(|w| w.tls_serialize_detached()).transpose().map_err(mls)?,
            epoch: group.epoch().as_u64(),
        })
    }

    /// Joins from a Welcome addressed to one of our KeyPackages.
    pub fn join(&mut self, welcome: &[u8]) -> Result<(), GroupError> {
        let message = MlsMessageIn::tls_deserialize_exact(welcome).map_err(|_| GroupError::Malformed)?;
        let MlsMessageBodyIn::Welcome(welcome) = message.extract() else {
            return Err(GroupError::Malformed);
        };
        let staged = StagedWelcome::new_from_welcome(&self.provider, &Self::join_config(), welcome, None)
            .map_err(mls)?;
        let group = staged.into_group(&self.provider).map_err(mls)?;
        for m in group.members() {
            member_identity(&m)?;
        }
        self.group = Some(group);
        Ok(())
    }

    /// Processes a commit or an application message from another member.
    pub fn process(&mut self, message: &[u8]) -> Result<Processed, GroupError> {
        let message = MlsMessageIn::tls_deserialize_exact(message).map_err(|_| GroupError::Malformed)?;
        let message = message.try_into_protocol_message().map_err(|_| GroupError::Malformed)?;
        let provider = &self.provider;
        let group = self.group.as_mut().filter(|g| g.is_active()).ok_or(GroupError::NoGroup)?;
        let processed = group.process_message(provider, message).map_err(mls)?;
        let sender = leaf_credential_identity(processed.credential())?;
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(Processed::Application { sender, data: app.into_bytes() })
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                for add in staged.add_proposals() {
                    leaf_identity(add.add_proposal().key_package().leaf_node())?;
                }
                let removed_self = staged.self_removed();
                group.merge_staged_commit(provider, *staged).map_err(mls)?;
                Ok(Processed::NewEpoch { epoch: group.epoch().as_u64(), removed_self })
            }
            ProcessedMessageContent::ProposalMessage(p) => {
                group.store_pending_proposal(provider.storage(), *p).map_err(mls)?;
                Ok(Processed::Proposal)
            }
            // External joins aren't allowed, and our own messages never
            // come back through `process` (we merge our commits at once).
            _ => Err(GroupError::Malformed),
        }
    }

    /// Seals an application message for every current member.
    pub fn encrypt(&mut self, data: &[u8]) -> Result<Vec<u8>, GroupError> {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().filter(|g| g.is_active()).ok_or(GroupError::NoGroup)?;
        group.create_message(provider, signer, data).map_err(mls)?.tls_serialize_detached().map_err(mls)
    }

    /// The current epoch.
    pub fn epoch(&self) -> Result<u64, GroupError> {
        Ok(self.group()?.epoch().as_u64())
    }

    /// Our leaf index -- what SFrame's KID calls the sender index.
    pub fn own_index(&self) -> Result<u32, GroupError> {
        Ok(self.group()?.own_leaf_index().u32())
    }

    /// Every member's leaf index and identity key, ourselves included.
    pub fn members(&self) -> Result<Vec<(u32, [u8; 32])>, GroupError> {
        self.group()?.members().map(|m| Ok((m.index.u32(), member_identity(&m)?))).collect()
    }

    /// RFC 9420 §8.5 `MLS-Exporter(label, context, len)` for the current
    /// epoch: a secret only this epoch's members can compute.
    pub fn export_secret(&self, label: &str, context: &[u8], len: usize) -> Result<Vec<u8>, GroupError> {
        self.group()?.export_secret(self.provider.crypto(), label, context, len).map_err(mls)
    }

    /// RFC 9605 §5.2's `base_key` for this epoch's SFrame keys.
    pub fn sframe_base_key(&self) -> Result<Vec<u8>, GroupError> {
        self.export_secret(crate::sframe::MLS_EXPORTER_LABEL, b"", crate::sframe::KEY_LEN)
    }

    /// A fresh [`SframeContext`](crate::sframe::SframeContext) keyed for
    /// the current epoch.
    pub fn sframe_context(&self) -> Result<crate::sframe::SframeContext, GroupError> {
        let mut ctx = crate::sframe::SframeContext::new(self.own_index()?);
        self.update_sframe(&mut ctx)?;
        Ok(ctx)
    }

    /// Installs the current epoch's key into `ctx`, after a commit.
    pub fn update_sframe(&self, ctx: &mut crate::sframe::SframeContext) -> Result<(), GroupError> {
        ctx.install_epoch(self.epoch()?, self.own_index()?, &self.sframe_base_key()?);
        Ok(())
    }
}

fn leaf_identity(leaf: &LeafNode) -> Result<[u8; 32], GroupError> {
    let id = leaf_credential_identity(leaf.credential())?;
    if leaf.signature_key().as_slice() != id {
        return Err(GroupError::CredentialMismatch);
    }
    Ok(id)
}

fn member_identity(m: &Member) -> Result<[u8; 32], GroupError> {
    let id = leaf_credential_identity(&m.credential)?;
    if m.signature_key != id {
        return Err(GroupError::CredentialMismatch);
    }
    Ok(id)
}

fn leaf_credential_identity(credential: &Credential) -> Result<[u8; 32], GroupError> {
    if credential.credential_type() != CredentialType::Basic {
        return Err(GroupError::CredentialMismatch);
    }
    credential.serialized_content().try_into().map_err(|_| GroupError::CredentialMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sframe::SframeContext;

    struct Person {
        key: [u8; 32],
        mls: GroupMember,
    }

    fn person() -> Person {
        let id = IdentityKeyPair::generate();
        Person { key: id.public().identity_key, mls: GroupMember::new(&id) }
    }

    fn deliver(commit: &Commit, to: &mut [&mut Person]) {
        for p in to {
            match p.mls.process(&commit.commit).unwrap() {
                Processed::NewEpoch { epoch, removed_self } => {
                    assert_eq!(epoch, commit.epoch);
                    assert!(!removed_self || !p.mls.in_group());
                }
                other => panic!("expected a commit, got {other:?}"),
            }
        }
    }

    #[test]
    fn three_members_add_remove_and_the_removed_one_is_locked_out() {
        let (mut alice, mut bob, mut carol) = (person(), person(), person());
        alice.mls.create().unwrap();
        assert_eq!(alice.mls.epoch().unwrap(), 0);

        // Alice adds Bob.
        let kp = bob.mls.key_package().unwrap();
        assert_eq!(alice.mls.key_package_identity(&kp).unwrap(), bob.key);
        let c1 = alice.mls.add(&[&kp]).unwrap();
        bob.mls.join(c1.welcome.as_ref().unwrap()).unwrap();
        assert_eq!(bob.mls.epoch().unwrap(), 1);

        // Then Carol; Bob processes the commit.
        let kp = carol.mls.key_package().unwrap();
        let c2 = alice.mls.add(&[&kp]).unwrap();
        deliver(&c2, &mut [&mut bob]);
        carol.mls.join(c2.welcome.as_ref().unwrap()).unwrap();
        let epoch2 = alice.mls.epoch().unwrap();
        assert_eq!(epoch2, 2);
        for p in [&alice, &bob, &carol] {
            assert_eq!(p.mls.epoch().unwrap(), epoch2);
            let mut keys: Vec<_> = p.mls.members().unwrap().into_iter().map(|(_, k)| k).collect();
            keys.sort();
            let mut want = vec![alice.key, bob.key, carol.key];
            want.sort();
            assert_eq!(keys, want);
        }
        // Same exported secret everywhere, so the same SFrame keys.
        let base = alice.mls.sframe_base_key().unwrap();
        assert_eq!(bob.mls.sframe_base_key().unwrap(), base);
        assert_eq!(carol.mls.sframe_base_key().unwrap(), base);

        // Application messages, with the sender's identity.
        let m = carol.mls.encrypt(b"hello group").unwrap();
        assert_eq!(alice.mls.process(&m).unwrap(), Processed::Application { sender: carol.key, data: b"hello group".to_vec() });
        assert_eq!(bob.mls.process(&m).unwrap(), Processed::Application { sender: carol.key, data: b"hello group".to_vec() });

        // SFrame between them in epoch 2; Carol can read it.
        let mut a_sf = alice.mls.sframe_context().unwrap();
        let mut c_sf = carol.mls.sframe_context().unwrap();
        let alice_index = alice.mls.own_index().unwrap();
        let frame = a_sf.seal(b"audio in epoch 2");
        assert_eq!(c_sf.open(alice_index, &frame).unwrap(), b"audio in epoch 2");

        // Alice removes Carol: a new epoch Carol never learns.
        let c3 = alice.mls.remove(&[carol.key]).unwrap();
        deliver(&c3, &mut [&mut bob]);
        match carol.mls.process(&c3.commit).unwrap() {
            Processed::NewEpoch { removed_self, .. } => assert!(removed_self),
            other => panic!("{other:?}"),
        }
        assert!(!carol.mls.in_group());
        assert_eq!(alice.mls.epoch().unwrap(), 3);
        assert_eq!(bob.mls.members().unwrap().len(), 2);

        // Carol can't read later group messages...
        let later = alice.mls.encrypt(b"after carol left").unwrap();
        assert!(carol.mls.process(&later).is_err());
        assert_eq!(bob.mls.process(&later).unwrap(), Processed::Application { sender: alice.key, data: b"after carol left".to_vec() });
        // ...nor derive the new epoch's media key, nor open new audio.
        assert!(carol.mls.sframe_base_key().is_err());
        let new_base = alice.mls.sframe_base_key().unwrap();
        assert_ne!(new_base, base);
        alice.mls.update_sframe(&mut a_sf).unwrap();
        let mut b_sf = bob.mls.sframe_context().unwrap();
        let frame = a_sf.seal(b"audio in epoch 3");
        assert_eq!(b_sf.open(alice_index, &frame).unwrap(), b"audio in epoch 3");
        assert!(c_sf.open(alice_index, &frame).is_err());
    }

    #[test]
    fn a_key_package_whose_credential_and_signing_key_differ_is_refused() {
        // Mallory signs a KeyPackage with her own key but names Bob.
        let mallory = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let provider = OpenMlsRustCrypto::default();
        let signer = IdentitySigner(IdentityKeyPair::from_master_seed(mallory.master_seed()));
        let forged = CredentialWithKey {
            credential: BasicCredential::new(bob.public().identity_key.to_vec()).into(),
            signature_key: mallory.public().identity_key.to_vec().into(),
        };
        let kp = KeyPackage::builder().build(CIPHERSUITE, &provider, &signer, forged).unwrap();
        let bytes = MlsMessageOut::from(kp.key_package().clone()).tls_serialize_detached().unwrap();

        let mut alice = person();
        alice.mls.create().unwrap();
        assert!(matches!(alice.mls.key_package_identity(&bytes), Err(GroupError::CredentialMismatch)));
        assert!(matches!(alice.mls.add(&[&bytes]), Err(GroupError::CredentialMismatch)));
        assert_eq!(alice.mls.epoch().unwrap(), 0);
    }

    #[test]
    fn garbage_is_refused_not_panicked_on() {
        let mut alice = person();
        assert!(matches!(alice.mls.process(b"junk"), Err(GroupError::Malformed)));
        assert!(alice.mls.join(b"junk").is_err());
        alice.mls.create().unwrap();
        assert!(alice.mls.add(&[b"junk"]).is_err());
        assert!(matches!(alice.mls.remove(&[[1; 32]]), Err(GroupError::NotAMember)));
        let _ = SframeContext::new(0);
    }
}
