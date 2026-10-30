//! Devices and cross-signing (spec §3 "Multi-device -- new devices added
//! by approving them from an existing device").
//!
//! The rule this module enforces: **an identity's devices are exactly the
//! ones an already-trusted device has signed into the list.** Holding the
//! recovery phrase makes you the identity, but it does not retroactively
//! make a new laptop a member of the device list -- that takes a signature
//! from the master key, which in practice means approving it from a device
//! you already have.
//!
//! Why that matters, and why it is not merely bureaucracy: the encrypted
//! backup key is sealed to devices in this list (see [`crate::backup`]).
//! So the list is what decides who can read history, and adding yourself
//! to it is the one action that cannot be done from nothing.
//!
//! Shape:
//!
//! - A device has its own keypair -- the same [`PublicIdentity`] shape as
//!   an account, reusing the same signed-prekey machinery. Devices talk
//!   and receive sealed data using their own keys, never the master.
//! - A [`DeviceCredential`] is that device's public key plus a signature
//!   from the **master identity** vouching for it. That signature is the
//!   cross-signing step.
//! - A [`SignedDeviceList`] is the full authorized set, signed and
//!   sequenced like a route record. Revoking a device means publishing a
//!   shorter list with a higher sequence.
//!
//! Revocation is by omission rather than by tombstone because the list is
//! the authority: a peer holding sequence 7 does not care what was in
//! sequence 6, only who is authorized now.
//!
//! **Signing as a device.** Every device of an account holds the master
//! seed, so a device that has been removed can still sign as the account
//! -- including a new device list. Two things keep it out:
//!
//! - What a device sends contacts carries a [`DeviceProof`]: the device's
//!   own signature, which a contact checks against the list it holds
//!   ([`SignedDeviceList::check`]). A removed device's key is not on it.
//! - A new list is only taken in place of a known one when a device on
//!   the *known* list endorses it ([`CarriedList`], [`judge`]). The master
//!   signature alone -- which a removed device can still make -- is not
//!   enough to change who speaks for the account.

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair, PublicIdentity};

// Frozen under the project's old name -- see "Frozen labels" in `lib.rs`.
const CREDENTIAL_CONTEXT: &[u8] = b"voip-app/device/credential/v1";
const LIST_CONTEXT: &[u8] = b"voip-app/device/list/v1";
/// Domain separation for a device's own signatures ([`DeviceProof`]).
const PROOF_CONTEXT: &[u8] = b"stoop/device/proof/v1";
/// What a [`DeviceProof`] endorsing a device list is made for.
const ENDORSE_PURPOSE: &[u8] = b"list";

/// Upper bound on devices in one list, so a hostile list can't make a peer
/// allocate without limit.
const MAX_DEVICES: usize = 64;
/// Upper bound on a device's display name.
const MAX_NAME_LEN: usize = 64;

/// A device's own long-term keypair.
///
/// Structurally identical to an account identity -- same Ed25519 signing
/// key, same signed X25519 prekey -- because a device needs to do the same
/// two things: sign, and receive sealed data. Reusing the type means the
/// prekey-binding check is the same tested code path.
pub type DeviceKeyPair = IdentityKeyPair;

/// One authorized device: its public keys, a human label, and the master
/// identity's signature over both.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceCredential {
    /// The device's own public keys: what siblings seal to and what its
    /// signatures verify under.
    pub device: PublicIdentity,
    /// What the user sees -- "Desktop", "Phone". Not security-relevant and
    /// not unique; the device key is the identifier.
    pub name: String,
    /// When it was approved, unix seconds. Shown, and signed so it can't be
    /// changed afterwards.
    pub added_at_secs: u64,
    /// The master identity vouching for this device. This is the
    /// cross-signature.
    pub master_signature: [u8; 64],
}

impl DeviceCredential {
    /// Bytes the master signature covers.
    fn signing_bytes(device: &PublicIdentity, name: &str, added_at_secs: u64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(CREDENTIAL_CONTEXT);
        out.extend_from_slice(&device.encode());
        out.extend_from_slice(&(name.len() as u32).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&added_at_secs.to_be_bytes());
        out
    }

    /// Approves a device, signing it into the identity.
    ///
    /// This is the operation that requires an existing trusted device:
    /// it needs the master signing key, which only an already-set-up
    /// device (or a freshly recovered phrase) holds.
    pub fn approve(
        master: &IdentityKeyPair,
        device: PublicIdentity,
        name: &str,
        added_at_secs: u64,
    ) -> Result<Self, CryptoError> {
        if name.len() > MAX_NAME_LEN {
            return Err(CryptoError::MalformedDeviceList);
        }
        // A device whose own prekey signature is broken must never be
        // signed into the list -- doing so would launder a malformed key
        // behind a valid master signature.
        device.verify()?;

        let signature = master.sign(&Self::signing_bytes(&device, name, added_at_secs));
        Ok(Self { device, name: name.to_string(), added_at_secs, master_signature: signature })
    }

    /// Checks this device really was approved by `identity_key`.
    pub fn verify(&self, identity_key: &[u8; 32]) -> Result<(), CryptoError> {
        self.device.verify()?;
        verify_signature(
            identity_key,
            &Self::signing_bytes(&self.device, &self.name, self.added_at_secs),
            &self.master_signature,
        )
    }
}

/// The set of devices currently authorized for an identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeviceList {
    /// The account (master identity key) these devices speak for.
    pub identity_key: [u8; 32],
    /// Monotonic, same rollback protection as a route record: without it,
    /// a revoked device could replay an older list that still contains it.
    pub sequence: u64,
    /// The authorized devices, in the order they were added.
    pub devices: Vec<DeviceCredential>,
}

impl DeviceList {
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(LIST_CONTEXT);
        out.extend_from_slice(&self.identity_key);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&(self.devices.len() as u32).to_be_bytes());
        for device in &self.devices {
            out.extend_from_slice(&device.device.encode());
            out.extend_from_slice(&(device.name.len() as u32).to_be_bytes());
            out.extend_from_slice(device.name.as_bytes());
            out.extend_from_slice(&device.added_at_secs.to_be_bytes());
            out.extend_from_slice(&device.master_signature);
        }
        out
    }

    /// Signs the list with the master identity it names. Refuses a list
    /// naming another identity, or one longer than a peer would accept.
    pub fn sign(self, master: &IdentityKeyPair) -> Result<SignedDeviceList, CryptoError> {
        if self.identity_key != master.public().identity_key {
            return Err(CryptoError::MalformedIdentity);
        }
        if self.devices.len() > MAX_DEVICES {
            return Err(CryptoError::MalformedDeviceList);
        }
        let signature = master.sign(&self.signing_bytes());
        Ok(SignedDeviceList { list: self, signature })
    }
}

/// A device list plus the master signature over it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignedDeviceList {
    /// What was signed.
    pub list: DeviceList,
    /// The master identity's signature over [`DeviceList`]'s signing bytes.
    pub signature: [u8; 64],
}

impl SignedDeviceList {
    /// Verifies the list signature *and* every credential inside it.
    ///
    /// Both halves are needed. The list signature proves the set is the
    /// one the identity published; each credential's signature proves the
    /// individual device was approved rather than spliced in.
    pub fn verify(&self) -> Result<(), CryptoError> {
        verify_signature(
            &self.list.identity_key,
            &self.list.signing_bytes(),
            &self.signature,
        )?;
        for device in &self.list.devices {
            device.verify(&self.list.identity_key)?;
        }
        Ok(())
    }

    /// Verifies, then checks this list supersedes what is already held.
    pub fn accept(&self, known_sequence: Option<u64>) -> Result<(), CryptoError> {
        self.verify()?;
        if let Some(known) = known_sequence {
            if self.list.sequence <= known {
                return Err(CryptoError::StaleDeviceList {
                    offered: self.list.sequence,
                    known,
                });
            }
        }
        Ok(())
    }

    /// Whether a given device key is currently authorized.
    ///
    /// The question [`crate::backup`] asks before sealing anything, and
    /// the question a peer asks before accepting a message from a device
    /// claiming to speak for this identity.
    pub fn authorizes(&self, device_key: &[u8; 32]) -> bool {
        self.list
            .devices
            .iter()
            .any(|d| &d.device.identity_key == device_key)
    }

    /// Every device the list authorizes, in the order they were added.
    pub fn devices(&self) -> &[DeviceCredential] {
        &self.list.devices
    }

    /// The credential for `device_key`, if the list authorizes it.
    pub fn credential(&self, device_key: &[u8; 32]) -> Option<&DeviceCredential> {
        self.list.devices.iter().find(|d| &d.device.identity_key == device_key)
    }

    /// The next list with `credential` added (or, when a device with the
    /// same key is already listed, put in its place), signed under the
    /// next sequence number.
    ///
    /// `credential` must already verify under this identity -- one made by
    /// [`DeviceCredential::approve`] with the same master key does.
    pub fn with_device(&self, master: &IdentityKeyPair, credential: DeviceCredential) -> Result<SignedDeviceList, CryptoError> {
        credential.verify(&self.list.identity_key)?;
        let mut devices: Vec<DeviceCredential> =
            self.list.devices.iter().filter(|d| d.device.identity_key != credential.device.identity_key).cloned().collect();
        devices.push(credential);
        DeviceList { identity_key: self.list.identity_key, sequence: self.next_sequence()?, devices }.sign(master)
    }

    /// The next list without `device_key`: revoking that device. Signed
    /// under the next sequence number, so anyone holding this list refuses
    /// the older one that still named it.
    pub fn without_device(&self, master: &IdentityKeyPair, device_key: &[u8; 32]) -> Result<SignedDeviceList, CryptoError> {
        let devices = self.list.devices.iter().filter(|d| &d.device.identity_key != device_key).cloned().collect();
        DeviceList { identity_key: self.list.identity_key, sequence: self.next_sequence()?, devices }.sign(master)
    }

    /// One past this list's sequence; an error only if it would wrap,
    /// which would let the oldest list win again.
    fn next_sequence(&self) -> Result<u64, CryptoError> {
        self.list.sequence.checked_add(1).ok_or(CryptoError::MalformedDeviceList)
    }

    /// Wire and file encoding:
    ///
    /// ```text
    /// identity_key[32] sequence u64 BE count u32 BE
    /// count × ( device[PublicIdentity::ENCODED_LEN] name_len u16 BE name
    ///           added_at u64 BE master_signature[64] )
    /// list_signature[64]
    /// ```
    ///
    /// Fixed-width fields and explicit lengths, like [`PublicIdentity::encode`]:
    /// the bytes are signed, so their layout must never depend on a
    /// serialization library's choices.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.list.identity_key);
        out.extend_from_slice(&self.list.sequence.to_be_bytes());
        out.extend_from_slice(&(self.list.devices.len() as u32).to_be_bytes());
        for d in &self.list.devices {
            out.extend_from_slice(&d.device.encode());
            // Names are capped at MAX_NAME_LEN when approved, so u16 holds them.
            out.extend_from_slice(&(d.name.len() as u16).to_be_bytes());
            out.extend_from_slice(d.name.as_bytes());
            out.extend_from_slice(&d.added_at_secs.to_be_bytes());
            out.extend_from_slice(&d.master_signature);
        }
        out.extend_from_slice(&self.signature);
        out
    }

    /// Reads [`SignedDeviceList::encode`]'s layout. Only the shape is
    /// checked here; call [`SignedDeviceList::verify`] or
    /// [`SignedDeviceList::accept`] before trusting what it says.
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let mut r = Reader { bytes, at: 0 };
        let identity_key = r.array::<32>()?;
        let sequence = u64::from_be_bytes(r.array::<8>()?);
        let count = u32::from_be_bytes(r.array::<4>()?) as usize;
        // Checked before allocating, so a hostile count can't ask for memory.
        if count > MAX_DEVICES {
            return Err(CryptoError::MalformedDeviceList);
        }
        let mut devices = Vec::with_capacity(count);
        for _ in 0..count {
            let device = PublicIdentity::decode(r.take(PublicIdentity::ENCODED_LEN)?)?;
            let name_len = u16::from_be_bytes(r.array::<2>()?) as usize;
            if name_len > MAX_NAME_LEN {
                return Err(CryptoError::MalformedDeviceList);
            }
            let name = String::from_utf8(r.take(name_len)?.to_vec()).map_err(|_| CryptoError::MalformedDeviceList)?;
            let added_at_secs = u64::from_be_bytes(r.array::<8>()?);
            let master_signature = r.array::<64>()?;
            devices.push(DeviceCredential { device, name, added_at_secs, master_signature });
        }
        let signature = r.array::<64>()?;
        if r.at != bytes.len() {
            return Err(CryptoError::MalformedDeviceList);
        }
        Ok(Self { list: DeviceList { identity_key, sequence, devices }, signature })
    }
}

/// A device's own signature over something it sends for the account:
/// which device, and its signature. A contact believes it only from a
/// device on the account's list ([`SignedDeviceList::check`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceProof {
    /// The signing device's key.
    pub device: [u8; 32],
    /// The device's signature over the purpose and message.
    pub signature: [u8; 64],
}

impl DeviceProof {
    /// Encoded length: device key, signature.
    pub const ENCODED_LEN: usize = 32 + 64;

    /// `device` signs `message`, made for `purpose` -- a short label
    /// ("mail", "hello", "route") so a proof for one thing can't be passed
    /// off as another.
    pub fn sign(device: &DeviceKeyPair, purpose: &[u8], message: &[u8]) -> Self {
        Self { device: device.public().identity_key, signature: device.sign(&Self::signed_bytes(purpose, message)) }
    }

    /// Checks the signature alone -- not whether the device is listed.
    pub fn verify(&self, purpose: &[u8], message: &[u8]) -> Result<(), CryptoError> {
        verify_signature(&self.device, &Self::signed_bytes(purpose, message), &self.signature)
    }

    /// What the signature covers.
    fn signed_bytes(purpose: &[u8], message: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(PROOF_CONTEXT.len() + 1 + purpose.len() + message.len());
        out.extend_from_slice(PROOF_CONTEXT);
        // Purposes are short labels; the length keeps them from running
        // into the message.
        out.push(purpose.len() as u8);
        out.extend_from_slice(purpose);
        out.extend_from_slice(message);
        out
    }

    /// `device[32] signature[64]`.
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..32].copy_from_slice(&self.device);
        out[32..].copy_from_slice(&self.signature);
        out
    }

    /// Reads [`DeviceProof::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: &[u8; Self::ENCODED_LEN] = bytes.try_into().map_err(|_| CryptoError::MalformedDeviceList)?;
        Ok(Self { device: bytes[..32].try_into().expect("32 bytes"), signature: bytes[32..].try_into().expect("64 bytes") })
    }
}

impl SignedDeviceList {
    /// Checks `proof` was made for `purpose` over `message` by a device
    /// this list authorizes, and returns that device's credential.
    pub fn check(&self, proof: &DeviceProof, purpose: &[u8], message: &[u8]) -> Result<&DeviceCredential, CryptoError> {
        let credential = self.credential(&proof.device).ok_or(CryptoError::DeviceNotAuthorized)?;
        proof.verify(purpose, message)?;
        Ok(credential)
    }

    /// This list's endorsement by `device`: the device vouching that this
    /// is the account's list now (see [`judge`]).
    pub fn endorse(&self, device: &DeviceKeyPair) -> DeviceProof {
        DeviceProof::sign(device, ENDORSE_PURPOSE, &self.encode())
    }

    /// Whether `proof` is a valid endorsement of this list by one of the
    /// devices on it.
    pub fn is_endorsed_by(&self, proof: &DeviceProof) -> bool {
        self.check(proof, ENDORSE_PURPOSE, &self.encode()).is_ok()
    }
}

/// A device list as it travels to contacts: the list, and the endorsements
/// of the devices on it that have seen it.
///
/// A contact holding an older list takes this one only when a device on
/// the older list endorses it ([`judge`]). Each device endorses every list
/// it accepts, so as long as one device the contact knows is still on the
/// account, the change goes through.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CarriedList {
    /// The account's list.
    pub list: SignedDeviceList,
    /// Endorsements of `list` by devices on it.
    pub endorsements: Vec<DeviceProof>,
}

impl CarriedList {
    /// `list_len u32 BE, list, count u8, count × DeviceProof`.
    pub fn encode(&self) -> Vec<u8> {
        let list = self.list.encode();
        let mut out = Vec::with_capacity(4 + list.len() + 1 + self.endorsements.len() * DeviceProof::ENCODED_LEN);
        out.extend_from_slice(&(list.len() as u32).to_be_bytes());
        out.extend_from_slice(&list);
        // A list holds at most MAX_DEVICES (64), so at most that many endorse it.
        out.push(self.endorsements.len().min(MAX_DEVICES) as u8);
        for proof in self.endorsements.iter().take(MAX_DEVICES) {
            out.extend_from_slice(&proof.encode());
        }
        out
    }

    /// Reads [`CarriedList::encode`]. Only the shape is checked; [`judge`]
    /// checks the signatures.
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let mut r = Reader { bytes, at: 0 };
        let len = u32::from_be_bytes(r.array::<4>()?) as usize;
        let list = SignedDeviceList::decode(r.take(len)?)?;
        let count = r.array::<1>()?[0] as usize;
        if count > MAX_DEVICES {
            return Err(CryptoError::MalformedDeviceList);
        }
        let endorsements = (0..count).map(|_| DeviceProof::decode(r.take(DeviceProof::ENCODED_LEN)?)).collect::<Result<Vec<_>, _>>()?;
        if r.at != bytes.len() {
            return Err(CryptoError::MalformedDeviceList);
        }
        Ok(Self { list, endorsements })
    }
}

/// What a contact should do with a device list it was offered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Judged {
    /// The list already held, or an older one: nothing to do.
    Unchanged,
    /// Newer, and endorsed by a device on the list held (or no list was
    /// held): take it.
    Accepted,
    /// Newer, but no device on the list held endorses it -- what a removed
    /// device holding the master seed could make, and also what restoring
    /// from the recovery phrase with no old device left makes. Only taken
    /// when the user says so, after checking with the person.
    Unendorsed,
}

/// Decides whether a contact should take `offered` in place of `known`.
/// An error is a list that isn't the account's at all (or is damaged).
pub fn judge(known: Option<&SignedDeviceList>, offered: &CarriedList) -> Result<Judged, CryptoError> {
    offered.list.verify()?;
    let Some(known) = known else { return Ok(Judged::Accepted) };
    if offered.list.list.identity_key != known.list.identity_key {
        return Err(CryptoError::MalformedIdentity);
    }
    if offered.list.list.sequence <= known.list.sequence {
        return Ok(Judged::Unchanged);
    }
    let endorsed = offered.endorsements.iter().any(|p| known.authorizes(&p.device) && offered.list.is_endorsed_by(p));
    Ok(if endorsed { Judged::Accepted } else { Judged::Unendorsed })
}

/// A cursor over encoded bytes for [`SignedDeviceList::decode`]; every
/// short read is a malformed list rather than a panic.
struct Reader<'a> {
    /// The whole encoding.
    bytes: &'a [u8],
    /// How far has been read.
    at: usize,
}

impl<'a> Reader<'a> {
    /// The next `n` bytes.
    fn take(&mut self, n: usize) -> Result<&'a [u8], CryptoError> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len()).ok_or(CryptoError::MalformedDeviceList)?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    /// The next `N` bytes as an array.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], CryptoError> {
        Ok(self.take(N)?.try_into().expect("take returned N bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_of(
        master: &IdentityKeyPair,
        sequence: u64,
        devices: Vec<DeviceCredential>,
    ) -> SignedDeviceList {
        DeviceList { identity_key: master.public().identity_key, sequence, devices }
            .sign(master)
            .unwrap()
    }

    fn approve(master: &IdentityKeyPair, device: &DeviceKeyPair, name: &str) -> DeviceCredential {
        DeviceCredential::approve(master, device.public(), name, 1_700_000_000).unwrap()
    }

    #[test]
    fn an_approved_device_verifies_under_its_identity() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();

        let credential = approve(&master, &desktop, "Desktop");
        assert!(credential.verify(&master.public().identity_key).is_ok());
    }

    #[test]
    fn a_device_approved_by_someone_else_is_not_authorized_here() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        // Mallory can happily sign this device into *her* identity, but
        // that says nothing about ours.
        let credential = approve(&mallory, &device, "Phone");
        assert!(credential.verify(&master.public().identity_key).is_err());
    }

    #[test]
    fn a_signed_list_verifies_and_reports_its_members() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();

        let signed = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &phone, "Phone")],
        );

        assert!(signed.verify().is_ok());
        assert!(signed.authorizes(&desktop.public().identity_key));
        assert!(signed.authorizes(&phone.public().identity_key));
    }

    #[test]
    fn a_device_never_approved_is_not_authorized() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let stranger = DeviceKeyPair::generate();

        let signed = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]);

        // This is the property the whole module exists for: holding a
        // valid keypair is not membership. Someone must sign you in.
        assert!(!signed.authorizes(&stranger.public().identity_key));
    }

    #[test]
    fn splicing_an_unapproved_device_into_a_list_is_caught() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let intruder = DeviceKeyPair::generate();

        let mut signed = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]);
        // Mallory appends her own device, signed by herself.
        signed.list.devices.push(approve(&mallory, &intruder, "Intruder"));

        assert!(signed.verify().is_err());
    }

    #[test]
    fn revoking_a_device_is_publishing_a_shorter_list() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let lost_phone = DeviceKeyPair::generate();

        let before = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &lost_phone, "Phone")],
        );
        let after = list_of(&master, 2, vec![approve(&master, &desktop, "Desktop")]);

        assert!(before.authorizes(&lost_phone.public().identity_key));
        assert!(!after.authorizes(&lost_phone.public().identity_key));
        assert!(after.accept(Some(before.list.sequence)).is_ok());
    }

    #[test]
    fn a_revoked_device_cannot_replay_the_older_list_that_contained_it() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let lost_phone = DeviceKeyPair::generate();

        let before = list_of(
            &master,
            1,
            vec![approve(&master, &desktop, "Desktop"), approve(&master, &lost_phone, "Phone")],
        );

        // The old list is perfectly signed, so only the sequence check
        // stops a stolen phone from re-authorizing itself.
        assert!(matches!(
            before.accept(Some(2)),
            Err(CryptoError::StaleDeviceList { offered: 1, known: 2 })
        ));
    }

    #[test]
    fn a_list_cannot_be_signed_for_another_identity() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();

        let list = DeviceList {
            identity_key: master.public().identity_key,
            sequence: 1,
            devices: vec![],
        };
        assert!(list.sign(&mallory).is_err());
    }

    #[test]
    fn tampering_with_a_device_name_breaks_the_signature() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();

        let mut credential = approve(&master, &desktop, "Desktop");
        credential.name = "Definitely Desktop".to_string();

        assert!(credential.verify(&master.public().identity_key).is_err());
    }

    #[test]
    fn a_device_with_a_broken_prekey_signature_is_never_approved() {
        let master = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        let mut public = device.public();
        public.prekey_signature[0] ^= 0x01;

        assert!(DeviceCredential::approve(&master, public, "Broken", 0).is_err());
    }

    #[test]
    fn a_list_survives_encoding_and_still_verifies() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();
        let signed = list_of(&master, 3, vec![approve(&master, &desktop, "Desktop"), approve(&master, &phone, "Phone ✓")]);

        let back = SignedDeviceList::decode(&signed.encode()).unwrap();
        assert_eq!(back, signed);
        assert!(back.verify().is_ok());
    }

    #[test]
    fn a_damaged_or_padded_encoding_is_refused() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let bytes = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]).encode();

        assert!(SignedDeviceList::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(SignedDeviceList::decode(&padded).is_err());
        // A count far past the cap is refused before anything is allocated.
        let mut huge = bytes;
        huge[40..44].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(SignedDeviceList::decode(&huge).is_err());
    }

    #[test]
    fn adding_and_revoking_step_the_sequence() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();
        let one = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop")]);

        let two = one.with_device(&master, approve(&master, &phone, "Phone")).unwrap();
        assert_eq!(two.list.sequence, 2);
        assert!(two.accept(Some(1)).is_ok());
        assert!(two.authorizes(&phone.public().identity_key));
        assert_eq!(two.credential(&phone.public().identity_key).unwrap().name, "Phone");

        // Approving the same device again replaces it rather than listing it twice.
        let renamed = two.with_device(&master, approve(&master, &phone, "Pocket")).unwrap();
        assert_eq!(renamed.devices().len(), 2);
        assert_eq!(renamed.credential(&phone.public().identity_key).unwrap().name, "Pocket");

        let three = renamed.without_device(&master, &phone.public().identity_key).unwrap();
        assert!(!three.authorizes(&phone.public().identity_key));
        assert!(three.authorizes(&desktop.public().identity_key));
        assert!(three.accept(Some(renamed.list.sequence)).is_ok());
    }

    #[test]
    fn a_credential_from_another_identity_is_not_added() {
        let master = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let one = list_of(&master, 1, vec![]);
        let theirs = approve(&mallory, &DeviceKeyPair::generate(), "Intruder");
        assert!(one.with_device(&master, theirs).is_err());
    }

    #[test]
    fn an_empty_device_list_is_valid_and_authorizes_nobody() {
        let master = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();

        let signed = list_of(&master, 1, vec![]);

        assert!(signed.verify().is_ok());
        assert!(!signed.authorizes(&device.public().identity_key));
    }

    #[test]
    fn a_device_proof_holds_only_for_a_listed_device_and_its_purpose() {
        let master = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let removed = DeviceKeyPair::generate();
        let list = list_of(&master, 2, vec![approve(&master, &desktop, "Desktop")]);

        let proof = DeviceProof::sign(&desktop, b"mail", b"hello");
        assert_eq!(list.check(&proof, b"mail", b"hello").unwrap().name, "Desktop");
        assert!(list.check(&proof, b"hello", b"hello").is_err(), "made for something else");
        assert!(list.check(&proof, b"mail", b"changed").is_err());
        assert!(matches!(list.check(&DeviceProof::sign(&removed, b"mail", b"hello"), b"mail", b"hello"), Err(CryptoError::DeviceNotAuthorized)));
        // Nor can the account's master key stand in for a device.
        assert!(list.check(&DeviceProof::sign(&master, b"mail", b"hello"), b"mail", b"hello").is_err());
        assert_eq!(DeviceProof::decode(&proof.encode()).unwrap(), proof);
    }

    #[test]
    fn a_removed_device_cannot_get_a_list_of_its_own_accepted() {
        let master = IdentityKeyPair::generate();
        let (desktop, phone, stolen) = (DeviceKeyPair::generate(), DeviceKeyPair::generate(), DeviceKeyPair::generate());
        let one = list_of(&master, 1, vec![approve(&master, &desktop, "Desktop"), approve(&master, &stolen, "Laptop")]);
        // The desktop removes the laptop and adds a phone; each device
        // endorses the list once it has it.
        let two = one.without_device(&master, &stolen.public().identity_key).unwrap();
        let three = two.with_device(&master, approve(&master, &phone, "Phone")).unwrap();
        let carried = CarriedList { endorsements: vec![three.endorse(&desktop), three.endorse(&phone)], list: three.clone() };
        let carried = CarriedList::decode(&carried.encode()).unwrap();
        // A contact that only ever saw list one still takes it: the
        // desktop, which it knows, vouches for it.
        assert_eq!(judge(Some(&one), &carried).unwrap(), Judged::Accepted);
        assert_eq!(judge(None, &carried).unwrap(), Judged::Accepted);
        assert_eq!(judge(Some(&three), &carried).unwrap(), Judged::Unchanged);

        // The laptop still holds the master seed, so it can sign a list
        // putting itself back -- endorsed by itself, which counts for
        // nothing once a contact holds a list without it.
        let forged = three.with_device(&master, approve(&master, &stolen, "Laptop")).unwrap();
        let by_itself = CarriedList { endorsements: vec![forged.endorse(&stolen)], list: forged };
        assert_eq!(judge(Some(&three), &by_itself).unwrap(), Judged::Unendorsed);
        // And an endorsement copied from another list doesn't carry over.
        let copied = CarriedList { endorsements: vec![three.endorse(&desktop)], list: by_itself.list };
        assert_eq!(judge(Some(&three), &copied).unwrap(), Judged::Unendorsed);
    }
}
