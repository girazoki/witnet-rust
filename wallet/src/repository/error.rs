use failure::Fail;

use crate::{db, types};

#[derive(Debug, Fail)]
#[fail(display = "Database Error")]
pub enum Error {
    #[fail(display = "maximum key index reached for account")]
    IndexOverflow,
    #[fail(display = "overflow when calculating transaction value")]
    TransactionValueOverflow,
    #[fail(display = "transaction balance overflowed")]
    TransactionBalanceOverflow,
    #[fail(display = "transaction balance underflowed")]
    TransactionBalanceUnderflow,
    #[fail(display = "Invalid PKH: {}", _0)]
    Pkh(#[cause] types::PublicKeyHashParseError),
    #[fail(display = "not enough balance in account")]
    InsufficientBalance,
    #[fail(display = "maximum transaction id reached for account")]
    TransactionIdOverflow,
    #[fail(display = "mutex poison error")]
    MutexPoison,
    #[fail(display = "database failed: {}", _0)]
    Db(#[cause] db::Error),
    #[fail(display = "cipher failed {}", _0)]
    Cipher(#[cause] witnet_crypto::cipher::Error),
    #[fail(display = "{}", _0)]
    Failure(#[cause] failure::Error),
    #[fail(display = "key derivation failed: {}", _0)]
    KeyDerivation(#[cause] types::KeyDerivationError),
    #[fail(display = "transaction type not supported: {}", _0)]
    UnsupportedTransactionType(String),
    #[fail(display = "tally decode failed: {}", _0)]
    TallyRadDecode(String),
    #[fail(display = "reveal decode failed: {}", _0)]
    RevealRadDecode(String),
    #[fail(display = "transaction metadata type is wrong: {}", _0)]
    WrongMetadataType(String),
    #[fail(display = "block consolidation failed: {}", _0)]
    BlockConsolidation(String),
    #[fail(display = "hash parsing failed: {}", _0)]
    HashParseError(#[cause] types::HashParseError),
    #[fail(display = "failed because wallet is still syncing: {}", _0)]
    StillSyncing(String),
    #[fail(
        display = "Weight limit reached when trying to create a VTT of value {} nanoWits",
        _0
    )]
    MaximumVTTWeightReached(u64),
    #[fail(display = "Weight limit reached when trying to create DR {:?}", _0)]
    MaximumDRWeightReached(types::DataRequestOutput),
    #[fail(
        display = "The chosen fee ({} nanoWits) seems too large for a VTT transferring {} nanoWits",
        _0, _1
    )]
    VTTFeeTooLarge(u64, u64),
    #[fail(
        display = "The chosen fee ({} nanoWits) seems too large for sending the request {:?}",
        _0, _1
    )]
    DRFeeTooLarge(u64, types::DataRequestOutput),
}

impl From<failure::Error> for Error {
    fn from(err: failure::Error) -> Self {
        Error::Failure(err)
    }
}

impl From<witnet_crypto::cipher::Error> for Error {
    fn from(err: witnet_crypto::cipher::Error) -> Self {
        Error::Cipher(err)
    }
}

impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(_err: std::sync::PoisonError<T>) -> Self {
        Error::MutexPoison
    }
}

impl From<db::Error> for Error {
    fn from(err: db::Error) -> Self {
        Error::Db(err)
    }
}

impl From<types::KeyDerivationError> for Error {
    fn from(err: types::KeyDerivationError) -> Self {
        Error::KeyDerivation(err)
    }
}

impl From<types::PublicKeyHashParseError> for Error {
    fn from(err: types::PublicKeyHashParseError) -> Self {
        Error::Pkh(err)
    }
}

impl From<types::HashParseError> for Error {
    fn from(err: types::HashParseError) -> Self {
        Error::HashParseError(err)
    }
}
