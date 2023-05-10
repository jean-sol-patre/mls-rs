use crate::SqLiteDataStorageError;
use async_trait::async_trait;
use aws_mls_core::{
    aws_mls_codec::{MlsDecode, MlsEncode},
    crypto::{CipherSuite, SignatureSecretKey},
    identity::SigningIdentity,
    keychain::KeychainStorage,
};
use openssl::sha::sha512;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};

use aws_mls_core::aws_mls_codec;

#[derive(Debug, Clone)]
/// SQLite storage for MLS identities and secret keys.
pub struct SqLiteKeychainStorage {
    connection: Arc<Mutex<Connection>>,
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    aws_mls_codec::MlsEncode,
    aws_mls_codec::MlsDecode,
    aws_mls_codec::MlsSize,
)]
struct StoredSigningIdentity {
    identity: SigningIdentity,
    signer: SignatureSecretKey,
    cipher_suite: CipherSuite,
}

impl SqLiteKeychainStorage {
    pub(crate) fn new(connection: Connection) -> SqLiteKeychainStorage {
        SqLiteKeychainStorage {
            connection: Arc::new(Mutex::new(connection)),
        }
    }

    /// Insert a new signing identity into storage for use within MLS groups.
    pub fn insert(
        &self,
        identity: SigningIdentity,
        signer: SignatureSecretKey,
        cipher_suite: CipherSuite,
    ) -> Result<(), SqLiteDataStorageError> {
        let (id, _) = identifier_hash(&identity)?;
        self.insert_storage(
            id.as_slice(),
            StoredSigningIdentity {
                identity,
                signer,
                cipher_suite,
            },
        )
    }

    /// Delete an existing identity from storage.
    pub fn delete(&self, identity: &SigningIdentity) -> Result<(), SqLiteDataStorageError> {
        let (identifier, _) = identifier_hash(identity)?;
        self.delete_storage(&identifier)
    }

    fn insert_storage(
        &self,
        identifier: &[u8],
        identity_data: StoredSigningIdentity,
    ) -> Result<(), SqLiteDataStorageError> {
        let connection = self.connection.lock().unwrap();
        let StoredSigningIdentity {
            identity,
            signer,
            cipher_suite,
        } = identity_data;

        connection
            .execute(
                "INSERT INTO keychain (
                    identifier,
                    identity,
                    signature_secret_key,
                    cipher_suite
                ) VALUES (?,?,?,?)",
                params![
                    identifier,
                    identity
                        .mls_encode_to_vec()
                        .map_err(|e| SqLiteDataStorageError::DataConversionError(e.into()))?,
                    signer
                        .mls_encode_to_vec()
                        .map_err(|e| SqLiteDataStorageError::DataConversionError(e.into()))?,
                    u16::from(cipher_suite)
                ],
            )
            .map(|_| {})
            .map_err(|e| SqLiteDataStorageError::SqlEngineError(e.into()))
    }

    fn delete_storage(&self, identifier: &[u8]) -> Result<(), SqLiteDataStorageError> {
        let connection = self.connection.lock().unwrap();

        connection
            .execute(
                "DELETE FROM keychain WHERE identifier = ?",
                params![identifier],
            )
            .map(|_| {})
            .map_err(|e| SqLiteDataStorageError::SqlEngineError(e.into()))
    }

    /// Get all stored identities that match a ciphersuite.
    pub fn get_identities(
        &self,
        cipher_suite: CipherSuite,
    ) -> Result<Vec<SigningIdentity>, SqLiteDataStorageError> {
        let connection = self.connection.lock().unwrap();

        let mut stmt = connection
            .prepare("SELECT identity FROM keychain WHERE cipher_suite = ?")
            .map_err(|e| SqLiteDataStorageError::SqlEngineError(e.into()))?;

        let identities = stmt
            .query_map(params![u16::from(cipher_suite)], |row| {
                Ok(SigningIdentity::mls_decode(&mut row.get::<_, Vec<u8>>(0)?.as_slice()).unwrap())
            })
            .map_err(|e| SqLiteDataStorageError::SqlEngineError(e.into()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| SqLiteDataStorageError::DataConversionError(e.into()))?;

        Ok(identities)
    }

    fn signer(
        &self,
        identifier: &[u8],
    ) -> Result<Option<SignatureSecretKey>, SqLiteDataStorageError> {
        let connection = self.connection.lock().unwrap();

        connection
            .query_row(
                "SELECT signature_secret_key FROM keychain WHERE identifier = ?",
                params![identifier],
                |row| {
                    Ok(
                        SignatureSecretKey::mls_decode(&mut row.get::<_, Vec<u8>>(0)?.as_slice())
                            .unwrap(),
                    )
                },
            )
            .optional()
            .map_err(|e| SqLiteDataStorageError::SqlEngineError(e.into()))
    }
}

#[async_trait]
impl KeychainStorage for SqLiteKeychainStorage {
    type Error = SqLiteDataStorageError;

    async fn signer(
        &self,
        identity: &SigningIdentity,
    ) -> Result<Option<SignatureSecretKey>, Self::Error> {
        let (identifier, _) = identifier_hash(identity)?;
        Ok(self.signer(&identifier)?)
    }
}

fn identifier_hash(
    identity: &SigningIdentity,
) -> Result<(Vec<u8>, Vec<u8>), SqLiteDataStorageError> {
    let serialized_identity = identity
        .mls_encode_to_vec()
        .map_err(|e| SqLiteDataStorageError::DataConversionError(e.into()))?;

    let identifier = sha512(&serialized_identity);

    Ok((identifier.into(), serialized_identity))
}

#[cfg(test)]
mod tests {
    use aws_mls_core::{
        crypto::CipherSuite,
        identity::{BasicCredential, Credential, SigningIdentity},
    };

    use crate::{
        SqLiteDataStorageEngine,
        {connection_strategy::MemoryStrategy, test_utils::gen_rand_bytes},
    };

    use super::{SqLiteKeychainStorage, StoredSigningIdentity};

    const TEST_CIPHER_SUITE: CipherSuite = CipherSuite::CURVE25519_AES128;

    fn test_signing_identity() -> (Vec<u8>, StoredSigningIdentity) {
        let identifier = gen_rand_bytes(32);

        let identity = StoredSigningIdentity {
            identity: SigningIdentity {
                signature_key: gen_rand_bytes(1024).into(),
                credential: Credential::Basic(BasicCredential::new(gen_rand_bytes(1024))),
            },
            signer: gen_rand_bytes(256).into(),
            cipher_suite: TEST_CIPHER_SUITE,
        };

        (identifier, identity)
    }

    fn test_storage() -> SqLiteKeychainStorage {
        SqLiteDataStorageEngine::new(MemoryStrategy)
            .unwrap()
            .keychain_storage()
            .unwrap()
    }

    #[test]
    fn identity_insert() {
        let storage = test_storage();
        let (identifier, stored_identity) = test_signing_identity();

        storage
            .insert_storage(identifier.as_slice(), stored_identity.clone())
            .unwrap();

        let from_storage = storage.get_identities(TEST_CIPHER_SUITE).unwrap();

        assert_eq!(from_storage.len(), 1);
        assert_eq!(from_storage[0], stored_identity.identity);

        // Get just the signer
        let signer = storage.signer(&identifier).unwrap().unwrap();
        assert_eq!(stored_identity.signer, signer);
    }

    #[test]
    fn multiple_identities() {
        let storage = test_storage();
        let test_identities = (0..10).map(|_| test_signing_identity()).collect::<Vec<_>>();

        test_identities
            .clone()
            .into_iter()
            .for_each(|(identifier, identity)| {
                storage
                    .insert_storage(identifier.as_slice(), identity)
                    .unwrap();
            });

        let from_storage = storage.get_identities(TEST_CIPHER_SUITE).unwrap();

        from_storage.into_iter().for_each(|stored_identity| {
            assert!(test_identities
                .iter()
                .any(|item| { item.1.identity == stored_identity }))
        });
    }

    #[test]
    fn delete_identity() {
        let storage = test_storage();
        let (identifier, identity) = test_signing_identity();

        storage
            .insert_storage(identifier.as_slice(), identity)
            .unwrap();

        storage.delete_storage(&identifier).unwrap();

        assert!(storage
            .get_identities(TEST_CIPHER_SUITE)
            .unwrap()
            .is_empty());
    }
}
