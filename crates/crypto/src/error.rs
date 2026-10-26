#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("identity key or its encoding is malformed")]
    MalformedIdentity,
    #[error("the prekey offered is not signed by the identity claiming it")]
    IdentitySignatureInvalid,
    #[error("route record is malformed or truncated")]
    MalformedRouteRecord,
    #[error("route record sequence {offered} does not supersede the known sequence {known}")]
    StaleRouteRecord { offered: u64, known: u64 },
    #[error("device list is malformed or holds too many devices")]
    MalformedDeviceList,
    #[error("device list sequence {offered} does not supersede the known sequence {known}")]
    StaleDeviceList { offered: u64, known: u64 },
    #[error("recovery phrase is not a valid 24-word phrase")]
    InvalidRecoveryPhrase,
    #[error("that is not a valid recovery share")]
    InvalidShare,
    #[error("not enough different shares to rebuild the identity")]
    NotEnoughShares,
    #[error("these shares are from different splits, or some are missing")]
    SharesDoNotMatch,
    #[error("this backup key was sealed for a different device")]
    BackupNotSealedForThisDevice,
    #[error("DHT record is malformed, too large, or not filed under its own key")]
    MalformedDhtRecord,
    #[error("DHT record signature does not verify")]
    DhtSignatureInvalid,
    #[error("invite link is malformed")]
    MalformedInvite,
    #[error("decryption failed: wrong key, corrupted data, or forged ciphertext")]
    DecryptionFailed,
    #[error("counter {counter} was already consumed or is older than the retained window")]
    ReplayOrTooOld { counter: u64 },
    #[error("counter {counter} is {gap} messages ahead of the last received one, past the max skip of {max_skip}")]
    SkipTooFar {
        counter: u64,
        gap: u64,
        max_skip: u64,
    },
}
