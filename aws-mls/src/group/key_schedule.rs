use crate::group::secret_tree::SecretTreeError;
use crate::group::{GroupContext, MembershipTag, MembershipTagError, SecretTree};
use crate::psk::secret::PskSecret;
use crate::psk::{PreSharedKey, PskError};
use crate::serde_utils::vec_u8_as_base64::VecAsBase64;
use crate::tree_kem::path_secret::{PathSecret, PathSecretGenerator};
use crate::tree_kem::RatchetTreeError;
use crate::CipherSuiteProvider;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use aws_mls_codec::{MlsDecode, MlsEncode, MlsSize};
use serde_with::serde_as;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

#[cfg(feature = "external_commit")]
use crate::crypto::{HpkeContextR, HpkeContextS, HpkePublicKey, HpkeSecretKey};

use super::epoch::{EpochSecrets, SenderDataSecret};
use super::message_signature::AuthenticatedContent;

#[cfg(feature = "std")]
use std::error::Error;

#[cfg(not(feature = "std"))]
use core::error::Error;

#[derive(Error, Debug)]
pub enum KeyScheduleError {
    #[error(transparent)]
    SecretTreeError(#[from] SecretTreeError),
    #[error(transparent)]
    MlsCodecError(#[from] aws_mls_codec::Error),
    #[error(transparent)]
    PskSecretError(#[from] PskError),
    #[error("key derivation failure")]
    KeyDerivationFailure,
    #[error(transparent)]
    CipherSuiteProviderError(Box<dyn Error + Send + Sync + 'static>),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Zeroize, Default)]
#[zeroize(drop)]
pub struct KeySchedule {
    exporter_secret: Zeroizing<Vec<u8>>,
    pub authentication_secret: Zeroizing<Vec<u8>>,
    #[cfg(feature = "external_commit")]
    external_secret: Zeroizing<Vec<u8>>,
    membership_key: Zeroizing<Vec<u8>>,
    init_secret: InitSecret,
}

pub(crate) struct KeyScheduleDerivationResult {
    pub(crate) key_schedule: KeySchedule,
    pub(crate) confirmation_key: Zeroizing<Vec<u8>>,
    pub(crate) joiner_secret: JoinerSecret,
    pub(crate) epoch_secrets: EpochSecrets,
}

impl KeySchedule {
    #[cfg(feature = "external_commit")]
    pub fn new(init_secret: InitSecret) -> Self {
        let mut key_schedule = KeySchedule::default();
        key_schedule.init_secret = init_secret;
        key_schedule
    }

    #[cfg(feature = "external_commit")]
    pub fn derive_for_external<P: CipherSuiteProvider>(
        &self,
        kem_output: &[u8],
        cipher_suite: &P,
    ) -> Result<KeySchedule, KeyScheduleError> {
        let (secret, _public) = self.get_external_key_pair(cipher_suite)?;
        let init_secret = InitSecret::decode_for_external(cipher_suite, kem_output, &secret)?;
        Ok(KeySchedule::new(init_secret))
    }

    /// Returns the derived epoch as well as the joiner secret required for building welcome
    /// messages
    pub(crate) fn from_key_schedule<P: CipherSuiteProvider>(
        last_key_schedule: &KeySchedule,
        commit_secret: &CommitSecret,
        context: &GroupContext,
        secret_tree_size: u32,
        psk_secret: &PskSecret,
        cipher_suite_provider: &P,
    ) -> Result<KeyScheduleDerivationResult, KeyScheduleError> {
        let joiner_seed = cipher_suite_provider
            .kdf_extract(&last_key_schedule.init_secret.0, &commit_secret.0)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        let joiner_secret = kdf_expand_with_label(
            cipher_suite_provider,
            &joiner_seed,
            "joiner",
            &context.mls_encode_to_vec()?,
            None,
        )
        .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?
        .into();

        let key_schedule_result = Self::from_joiner(
            cipher_suite_provider,
            &joiner_secret,
            context,
            secret_tree_size,
            psk_secret,
        )?;

        Ok(KeyScheduleDerivationResult {
            key_schedule: key_schedule_result.key_schedule,
            confirmation_key: key_schedule_result.confirmation_key,
            joiner_secret,
            epoch_secrets: key_schedule_result.epoch_secrets,
        })
    }

    pub(crate) fn from_joiner<P: CipherSuiteProvider>(
        cipher_suite_provider: &P,
        joiner_secret: &JoinerSecret,
        context: &GroupContext,
        secret_tree_size: u32,
        psk_secret: &PskSecret,
    ) -> Result<KeyScheduleDerivationResult, KeyScheduleError> {
        let epoch_seed = get_pre_epoch_secret(cipher_suite_provider, psk_secret, joiner_secret)?;
        let context = context.mls_encode_to_vec()?;

        let epoch_secret =
            kdf_expand_with_label(cipher_suite_provider, &epoch_seed, "epoch", &context, None)
                .map(Zeroizing::new)
                .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        Self::from_epoch_secret(cipher_suite_provider, &epoch_secret, secret_tree_size)
    }

    pub(crate) fn from_random_epoch_secret<P: CipherSuiteProvider>(
        cipher_suite_provider: &P,
        secret_tree_size: u32,
    ) -> Result<KeyScheduleDerivationResult, KeyScheduleError> {
        let epoch_secret = cipher_suite_provider
            .random_bytes_vec(cipher_suite_provider.kdf_extract_size())
            .map(Zeroizing::new)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        Self::from_epoch_secret(cipher_suite_provider, &epoch_secret, secret_tree_size)
    }

    fn from_epoch_secret<P: CipherSuiteProvider>(
        cipher_suite_provider: &P,
        epoch_secret: &[u8],
        secret_tree_size: u32,
    ) -> Result<KeyScheduleDerivationResult, KeyScheduleError> {
        let secrets_producer = SecretsProducer::new(cipher_suite_provider, epoch_secret);

        let epoch_secrets = EpochSecrets {
            resumption_secret: PreSharedKey::from(secrets_producer.derive("resumption")?),
            sender_data_secret: SenderDataSecret::from(secrets_producer.derive("sender data")?),
            secret_tree: SecretTree::new(secret_tree_size, secrets_producer.derive("encryption")?),
        };

        let key_schedule = Self {
            exporter_secret: secrets_producer.derive("exporter")?,
            authentication_secret: secrets_producer.derive("authentication")?,
            #[cfg(feature = "external_commit")]
            external_secret: secrets_producer.derive("external")?,
            membership_key: secrets_producer.derive("membership")?,
            init_secret: InitSecret(secrets_producer.derive("init")?),
        };

        Ok(KeyScheduleDerivationResult {
            key_schedule,
            confirmation_key: secrets_producer.derive("confirm")?,
            joiner_secret: Zeroizing::new(vec![]).into(),
            epoch_secrets,
        })
    }

    pub fn export_secret<P: CipherSuiteProvider>(
        &self,
        label: &str,
        context: &[u8],
        len: usize,
        cipher_suite: &P,
    ) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
        let secret = kdf_derive_secret(cipher_suite, &self.exporter_secret, label)?;

        let context_hash = cipher_suite
            .hash(context)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        kdf_expand_with_label(cipher_suite, &secret, "exported", &context_hash, Some(len))
    }

    pub fn get_membership_tag<P: CipherSuiteProvider>(
        &self,
        content: &AuthenticatedContent,
        context: &GroupContext,
        cipher_suite_provider: &P,
    ) -> Result<MembershipTag, MembershipTagError> {
        MembershipTag::create(
            content,
            context,
            &self.membership_key,
            cipher_suite_provider,
        )
    }

    #[cfg(feature = "external_commit")]
    pub fn get_external_key_pair<P: CipherSuiteProvider>(
        &self,
        cipher_suite: &P,
    ) -> Result<(HpkeSecretKey, HpkePublicKey), KeyScheduleError> {
        cipher_suite
            .kem_derive(&self.external_secret)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
    }
}

#[derive(MlsEncode, MlsSize)]
struct Label<'a> {
    length: u16,
    #[mls_codec(with = "aws_mls_codec::byte_vec")]
    label: Vec<u8>,
    #[mls_codec(with = "aws_mls_codec::byte_vec")]
    context: &'a [u8],
}

impl<'a> Label<'a> {
    fn new(length: u16, label: &'a str, context: &'a [u8]) -> Self {
        Self {
            length,
            label: [b"MLS 1.0 ", label.as_bytes()].concat(),
            context,
        }
    }
}

pub(crate) fn kdf_expand_with_label<P: CipherSuiteProvider>(
    cipher_suite_provider: &P,
    secret: &[u8],
    label: &str,
    context: &[u8],
    len: Option<usize>,
) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
    let extract_size = cipher_suite_provider.kdf_extract_size();
    let len = len.unwrap_or(extract_size);
    let label = Label::new(len as u16, label, context);

    cipher_suite_provider
        .kdf_expand(secret, &label.mls_encode_to_vec()?, len)
        .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
}

pub(crate) fn kdf_derive_secret<P: CipherSuiteProvider>(
    cipher_suite_provider: &P,
    secret: &[u8],
    label: &str,
) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
    kdf_expand_with_label(cipher_suite_provider, secret, label, &[], None)
}

#[derive(Clone, Debug, PartialEq, MlsSize, MlsEncode, MlsDecode)]
pub(crate) struct JoinerSecret(#[mls_codec(with = "aws_mls_codec::byte_vec")] Zeroizing<Vec<u8>>);

impl From<Zeroizing<Vec<u8>>> for JoinerSecret {
    fn from(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

pub(crate) fn get_pre_epoch_secret<P: CipherSuiteProvider>(
    cipher_suite_provider: &P,
    psk_secret: &PskSecret,
    joiner_secret: &JoinerSecret,
) -> Result<Zeroizing<Vec<u8>>, PskError> {
    cipher_suite_provider
        .kdf_extract(&joiner_secret.0, psk_secret)
        .map_err(|e| PskError::CipherSuiteProviderError(e.into()))
}

struct SecretsProducer<'a, P: CipherSuiteProvider> {
    cipher_suite_provider: &'a P,
    epoch_secret: &'a [u8],
}

impl<'a, P: CipherSuiteProvider> SecretsProducer<'a, P> {
    fn new(cipher_suite_provider: &'a P, epoch_secret: &'a [u8]) -> Self {
        Self {
            cipher_suite_provider,
            epoch_secret,
        }
    }

    // TODO document somewhere in the crypto provider that the RFC defines the length of all secrets as
    // KDF extract size but then inputs secrets as MAC keys etc, therefore, we require that these
    // lengths match in the crypto provider
    fn derive(&self, label: &str) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
        kdf_derive_secret(self.cipher_suite_provider, self.epoch_secret, label)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
    }
}

#[cfg(feature = "external_commit")]
const EXPORTER_CONTEXT: &[u8] = b"MLS 1.0 external init secret";

#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize, Zeroize, Default)]
pub struct InitSecret(#[serde_as(as = "VecAsBase64")] Zeroizing<Vec<u8>>);

#[cfg(feature = "external_commit")]
impl InitSecret {
    /// Returns init secret and KEM output to be used when creating an external commit.
    pub fn encode_for_external<P: CipherSuiteProvider>(
        cipher_suite: &P,
        external_pub: &HpkePublicKey,
    ) -> Result<(Self, Vec<u8>), KeyScheduleError> {
        let (kem_output, context) = cipher_suite
            .hpke_setup_s(external_pub, &[])
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        let init_secret = context
            .export(EXPORTER_CONTEXT, cipher_suite.kdf_extract_size())
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        Ok((InitSecret(Zeroizing::new(init_secret)), kem_output))
    }

    pub fn decode_for_external<P: CipherSuiteProvider>(
        cipher_suite: &P,
        kem_output: &[u8],
        external_secret: &HpkeSecretKey,
    ) -> Result<Self, KeyScheduleError> {
        let context = cipher_suite
            .hpke_setup_r(kem_output, external_secret, &[])
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))?;

        context
            .export(EXPORTER_CONTEXT, cipher_suite.kdf_extract_size())
            .map(Zeroizing::new)
            .map(InitSecret)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Zeroize)]
#[zeroize(drop)]
pub struct CommitSecret(PathSecret);

impl CommitSecret {
    pub fn from_root_secret<P: CipherSuiteProvider>(
        cipher_suite_provider: &P,
        root_secret: Option<&PathSecret>,
    ) -> Result<Self, RatchetTreeError> {
        match root_secret {
            Some(root_secret) => {
                let mut generator =
                    PathSecretGenerator::starting_from(cipher_suite_provider, root_secret.clone());

                Ok(CommitSecret(generator.next_secret()?.path_secret))
            }
            None => Ok(Self::empty(cipher_suite_provider)),
        }
    }

    pub fn empty<P: CipherSuiteProvider>(cipher_suite_provider: &P) -> CommitSecret {
        CommitSecret(PathSecret::empty(cipher_suite_provider))
    }
}

pub(crate) struct WelcomeSecret<'a, P: CipherSuiteProvider> {
    cipher_suite: &'a P,
    key: Zeroizing<Vec<u8>>,
    nonce: Zeroizing<Vec<u8>>,
}

impl<'a, P: CipherSuiteProvider> WelcomeSecret<'a, P> {
    pub(crate) fn from_joiner_secret(
        cipher_suite: &'a P,
        joiner_secret: &JoinerSecret,
        psk_secret: &PskSecret,
    ) -> Result<Self, KeyScheduleError> {
        let welcome_secret = get_welcome_secret(cipher_suite, joiner_secret, psk_secret)?;

        let key_len = cipher_suite.aead_key_size();
        let key = kdf_expand_with_label(cipher_suite, &welcome_secret, "key", &[], Some(key_len))?;

        let nonce_len = cipher_suite.aead_nonce_size();

        let nonce =
            kdf_expand_with_label(cipher_suite, &welcome_secret, "nonce", &[], Some(nonce_len))?;

        Ok(Self {
            cipher_suite,
            key,
            nonce,
        })
    }

    pub(crate) fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, KeyScheduleError> {
        self.cipher_suite
            .aead_seal(&self.key, plaintext, None, &self.nonce)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
    }

    pub(crate) fn decrypt(
        &self,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
        self.cipher_suite
            .aead_open(&self.key, ciphertext, None, &self.nonce)
            .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
    }
}

fn get_welcome_secret<P: CipherSuiteProvider>(
    cipher_suite: &P,
    joiner_secret: &JoinerSecret,
    psk_secret: &PskSecret,
) -> Result<Zeroizing<Vec<u8>>, KeyScheduleError> {
    let epoch_seed = get_pre_epoch_secret(cipher_suite, psk_secret, joiner_secret)?;
    kdf_derive_secret(cipher_suite, &epoch_seed, "welcome")
}

#[cfg(test)]
pub(crate) mod test_utils {
    use alloc::vec;
    use alloc::vec::Vec;
    use aws_mls_core::crypto::CipherSuiteProvider;
    use zeroize::Zeroizing;

    use crate::{cipher_suite::CipherSuite, crypto::test_utils::test_cipher_suite_provider};

    use super::{CommitSecret, InitSecret, JoinerSecret, KeySchedule, KeyScheduleError};

    impl From<JoinerSecret> for Vec<u8> {
        fn from(mut value: JoinerSecret) -> Self {
            core::mem::take(&mut value.0)
        }
    }

    pub(crate) fn get_test_key_schedule(cipher_suite: CipherSuite) -> KeySchedule {
        let key_size = test_cipher_suite_provider(cipher_suite).kdf_extract_size();
        let fake_secret = Zeroizing::new(vec![1u8; key_size]);

        KeySchedule {
            exporter_secret: fake_secret.clone(),
            authentication_secret: fake_secret.clone(),
            #[cfg(feature = "external_commit")]
            external_secret: fake_secret.clone(),
            membership_key: fake_secret,
            init_secret: InitSecret::new(vec![0u8; key_size]),
        }
    }

    impl InitSecret {
        pub fn new(init_secret: Vec<u8>) -> Self {
            InitSecret(Zeroizing::new(init_secret))
        }

        pub fn random<P: CipherSuiteProvider>(cipher_suite: &P) -> Result<Self, KeyScheduleError> {
            cipher_suite
                .random_bytes_vec(cipher_suite.kdf_extract_size())
                .map(Zeroizing::new)
                .map(InitSecret)
                .map_err(|e| KeyScheduleError::CipherSuiteProviderError(e.into()))
        }
    }

    impl KeySchedule {
        pub fn set_membership_key(&mut self, key: Vec<u8>) {
            self.membership_key = Zeroizing::new(key)
        }
    }

    impl AsRef<[u8]> for CommitSecret {
        fn as_ref(&self) -> &[u8] {
            &self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::client::test_utils::TEST_PROTOCOL_VERSION;
    use crate::crypto::test_utils::{
        test_cipher_suite_provider, try_test_cipher_suite_provider, TestCryptoProvider,
    };
    use crate::group::internal::PskSecret;
    use crate::group::key_schedule::{
        get_welcome_secret, kdf_derive_secret, kdf_expand_with_label,
    };
    use crate::group::test_utils::random_bytes;
    use crate::group::{GroupContext, InitSecret};
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;
    use aws_mls_codec::MlsEncode;
    use aws_mls_core::crypto::CipherSuiteProvider;

    use aws_mls_core::extension::ExtensionList;
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;
    use zeroize::Zeroizing;

    use super::test_utils::get_test_key_schedule;
    use super::{CommitSecret, KeySchedule, KeyScheduleDerivationResult};

    #[derive(serde::Deserialize, serde::Serialize)]
    struct KeyScheduleTestCase {
        cipher_suite: u16,
        #[serde(with = "hex::serde")]
        group_id: Vec<u8>,
        #[serde(with = "hex::serde")]
        initial_init_secret: Vec<u8>,
        epochs: Vec<KeyScheduleEpoch>,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct KeyScheduleEpoch {
        #[serde(with = "hex::serde")]
        commit_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        psk_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        confirmed_transcript_hash: Vec<u8>,
        #[serde(with = "hex::serde")]
        tree_hash: Vec<u8>,

        #[serde(with = "hex::serde")]
        group_context: Vec<u8>,

        #[serde(with = "hex::serde")]
        joiner_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        welcome_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        init_secret: Vec<u8>,

        #[serde(with = "hex::serde")]
        sender_data_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        encryption_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        exporter_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        epoch_authenticator: Vec<u8>,
        #[cfg(feature = "external_commit")]
        #[serde(with = "hex::serde")]
        external_secret: Vec<u8>,
        #[serde(with = "hex::serde")]
        confirmation_key: Vec<u8>,
        #[serde(with = "hex::serde")]
        membership_key: Vec<u8>,
        #[serde(with = "hex::serde")]
        resumption_psk: Vec<u8>,

        #[cfg(feature = "external_commit")]
        #[serde(with = "hex::serde")]
        external_pub: Vec<u8>,

        exporter: KeyScheduleExporter,
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct KeyScheduleExporter {
        label: String,
        #[serde(with = "hex::serde")]
        context: Vec<u8>,
        length: usize,
        #[serde(with = "hex::serde")]
        secret: Vec<u8>,
    }

    #[test]
    fn test_key_schedule() {
        let test_cases: Vec<KeyScheduleTestCase> =
            load_test_cases!(key_schedule_test_vector, generate_key_schedule_tests());

        for test_case in test_cases {
            let Some(cs_provider) = try_test_cipher_suite_provider(test_case.cipher_suite) else {
                continue;
            };

            let mut key_schedule = get_test_key_schedule(cs_provider.cipher_suite());
            key_schedule.init_secret.0 = Zeroizing::new(test_case.initial_init_secret);

            for (i, epoch) in test_case.epochs.into_iter().enumerate() {
                let context = GroupContext {
                    protocol_version: TEST_PROTOCOL_VERSION,
                    cipher_suite: cs_provider.cipher_suite(),
                    group_id: test_case.group_id.clone(),
                    epoch: i as u64,
                    tree_hash: epoch.tree_hash,
                    confirmed_transcript_hash: epoch.confirmed_transcript_hash.into(),
                    extensions: ExtensionList::new(),
                };

                assert_eq!(context.mls_encode_to_vec().unwrap(), epoch.group_context);

                let psk = epoch.psk_secret.into();
                let commit = CommitSecret(epoch.commit_secret.into());

                let key_schedule_res = KeySchedule::from_key_schedule(
                    &key_schedule,
                    &commit,
                    &context,
                    32,
                    &psk,
                    &cs_provider,
                )
                .unwrap();

                key_schedule = key_schedule_res.key_schedule;

                let welcome =
                    get_welcome_secret(&cs_provider, &key_schedule_res.joiner_secret, &psk)
                        .unwrap();

                assert_eq!(*welcome, epoch.welcome_secret);

                let expected: Vec<u8> = key_schedule_res.joiner_secret.into();
                assert_eq!(epoch.joiner_secret, expected);

                assert_eq!(&key_schedule.init_secret.0.to_vec(), &epoch.init_secret);

                assert_eq!(
                    epoch.sender_data_secret,
                    *key_schedule_res.epoch_secrets.sender_data_secret.to_vec()
                );

                assert_eq!(
                    epoch.encryption_secret,
                    *key_schedule_res.epoch_secrets.secret_tree.get_root_secret()
                );

                assert_eq!(epoch.exporter_secret, key_schedule.exporter_secret.to_vec());

                assert_eq!(
                    epoch.epoch_authenticator,
                    key_schedule.authentication_secret.to_vec()
                );

                #[cfg(feature = "external_commit")]
                assert_eq!(epoch.external_secret, key_schedule.external_secret.to_vec());

                assert_eq!(
                    epoch.confirmation_key,
                    key_schedule_res.confirmation_key.to_vec()
                );

                assert_eq!(epoch.membership_key, key_schedule.membership_key.to_vec());

                let expected: Vec<u8> = key_schedule_res.epoch_secrets.resumption_secret.to_vec();
                assert_eq!(epoch.resumption_psk, expected);

                #[cfg(feature = "external_commit")]
                {
                    let (_external_sec, external_pub) =
                        key_schedule.get_external_key_pair(&cs_provider).unwrap();

                    assert_eq!(epoch.external_pub, *external_pub);
                }

                let exp = epoch.exporter;

                let exported = key_schedule
                    .export_secret(&exp.label, &exp.context, exp.length, &cs_provider)
                    .unwrap();

                assert_eq!(exported.to_vec(), exp.secret);
            }
        }
    }

    #[cfg(feature = "rfc_compliant")]
    fn generate_key_schedule_tests() -> Vec<KeyScheduleTestCase> {
        let mut test_cases = vec![];

        for cipher_suite in TestCryptoProvider::all_supported_cipher_suites() {
            let cs_provider = test_cipher_suite_provider(cipher_suite);
            let key_size = cs_provider.kdf_extract_size();

            let mut group_context = GroupContext {
                protocol_version: TEST_PROTOCOL_VERSION,
                cipher_suite: cs_provider.cipher_suite(),
                group_id: b"my group 5".to_vec(),
                epoch: 0,
                tree_hash: random_bytes(key_size),
                confirmed_transcript_hash: random_bytes(key_size).into(),
                extensions: Default::default(),
            };

            let initial_init_secret = InitSecret::random(&cs_provider).unwrap();
            let mut key_schedule = get_test_key_schedule(cs_provider.cipher_suite());
            key_schedule.init_secret = initial_init_secret.clone();

            let commit_secret = CommitSecret(random_bytes(key_size).into());
            let psk_secret = PskSecret::new(&cs_provider);

            let key_schedule_res = KeySchedule::from_key_schedule(
                &key_schedule,
                &commit_secret,
                &group_context,
                32,
                &psk_secret,
                &cs_provider,
            )
            .unwrap();

            key_schedule = key_schedule_res.key_schedule.clone();

            let epoch1 = KeyScheduleEpoch::new(
                key_schedule_res,
                psk_secret,
                commit_secret.0.to_vec(),
                &group_context,
                &cs_provider,
            );

            group_context.epoch += 1;
            group_context.confirmed_transcript_hash = random_bytes(key_size).into();
            group_context.tree_hash = random_bytes(key_size);

            let commit_secret = CommitSecret(random_bytes(key_size).into());
            let psk_secret = PskSecret::new(&cs_provider);

            let key_schedule_res = KeySchedule::from_key_schedule(
                &key_schedule,
                &commit_secret,
                &group_context,
                32,
                &psk_secret,
                &cs_provider,
            )
            .unwrap();

            let epoch2 = KeyScheduleEpoch::new(
                key_schedule_res,
                psk_secret,
                commit_secret.0.to_vec(),
                &group_context,
                &cs_provider,
            );

            let test_case = KeyScheduleTestCase {
                cipher_suite: cs_provider.cipher_suite().into(),
                group_id: group_context.group_id.clone(),
                initial_init_secret: initial_init_secret.0.to_vec(),
                epochs: vec![epoch1, epoch2],
            };

            test_cases.push(test_case);
        }

        test_cases
    }

    #[cfg(not(feature = "rfc_compliant"))]
    fn generate_key_schedule_tests() -> Vec<KeyScheduleTestCase> {
        panic!("key schedule test vectors can only be generated with the feature \"rfc_compliant\"")
    }

    impl KeyScheduleEpoch {
        fn new<P: CipherSuiteProvider>(
            key_schedule_res: KeyScheduleDerivationResult,
            psk_secret: PskSecret,
            commit_secret: Vec<u8>,
            group_context: &GroupContext,
            cs: &P,
        ) -> Self {
            #[cfg(feature = "external_commit")]
            let (_external_sec, external_pub) = key_schedule_res
                .key_schedule
                .get_external_key_pair(cs)
                .unwrap();

            let mut exporter = KeyScheduleExporter {
                label: "exporter label 15".to_string(),
                context: b"exporter context".to_vec(),
                length: 64,
                secret: vec![],
            };

            exporter.secret = key_schedule_res
                .key_schedule
                .export_secret(&exporter.label, &exporter.context, exporter.length, cs)
                .unwrap()
                .to_vec();

            let welcome_secret =
                get_welcome_secret(cs, &key_schedule_res.joiner_secret, &psk_secret)
                    .unwrap()
                    .to_vec();

            KeyScheduleEpoch {
                commit_secret,
                welcome_secret,
                psk_secret: psk_secret.to_vec(),
                group_context: group_context.mls_encode_to_vec().unwrap(),
                joiner_secret: key_schedule_res.joiner_secret.into(),
                init_secret: key_schedule_res.key_schedule.init_secret.0.to_vec(),
                sender_data_secret: key_schedule_res.epoch_secrets.sender_data_secret.to_vec(),
                encryption_secret: key_schedule_res.epoch_secrets.secret_tree.get_root_secret(),
                exporter_secret: key_schedule_res.key_schedule.exporter_secret.to_vec(),
                epoch_authenticator: key_schedule_res.key_schedule.authentication_secret.to_vec(),
                #[cfg(feature = "external_commit")]
                external_secret: key_schedule_res.key_schedule.external_secret.to_vec(),
                confirmation_key: key_schedule_res.confirmation_key.to_vec(),
                membership_key: key_schedule_res.key_schedule.membership_key.to_vec(),
                resumption_psk: key_schedule_res.epoch_secrets.resumption_secret.to_vec(),
                #[cfg(feature = "external_commit")]
                external_pub: external_pub.to_vec(),
                exporter,
                confirmed_transcript_hash: group_context.confirmed_transcript_hash.to_vec(),
                tree_hash: group_context.tree_hash.clone(),
            }
        }
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct ExpandWithLabelTestCase {
        #[serde(with = "hex::serde")]
        secret: Vec<u8>,
        label: String,
        #[serde(with = "hex::serde")]
        context: Vec<u8>,
        length: usize,
        #[serde(with = "hex::serde")]
        out: Vec<u8>,
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct DeriveSecretTestCase {
        #[serde(with = "hex::serde")]
        secret: Vec<u8>,
        label: String,
        #[serde(with = "hex::serde")]
        out: Vec<u8>,
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    pub struct InteropTestCase {
        cipher_suite: u16,
        expand_with_label: ExpandWithLabelTestCase,
        derive_secret: DeriveSecretTestCase,
    }

    #[test]
    fn test_basic_crypto_test_vectors() {
        // The test vector can be found here https://github.com/mlswg/mls-implementations/blob/main/test-vectors/crypto-basics.json
        let test_cases: Vec<InteropTestCase> =
            load_test_cases!(basic_crypto, Vec::<InteropTestCase>::new());

        test_cases.into_iter().for_each(|test_case| {
            if let Some(cs) = try_test_cipher_suite_provider(test_case.cipher_suite) {
                let test_exp = &test_case.expand_with_label;

                let computed = kdf_expand_with_label(
                    &cs,
                    &test_exp.secret,
                    &test_exp.label,
                    &test_exp.context,
                    Some(test_exp.length),
                )
                .unwrap();

                assert_eq!(&computed.to_vec(), &test_exp.out);

                let test_derive = &test_case.derive_secret;

                let computed =
                    kdf_derive_secret(&cs, &test_derive.secret, &test_derive.label).unwrap();

                assert_eq!(&computed.to_vec(), &test_derive.out);
            }
        })
    }
}
