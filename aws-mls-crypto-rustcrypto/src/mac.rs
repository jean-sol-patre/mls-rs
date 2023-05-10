use aws_mls_core::crypto::CipherSuite;
use hmac::{
    digest::{crypto_common::BlockSizeUser, FixedOutputReset},
    Mac, SimpleHmac,
};
use sha2::{Digest, Sha256, Sha384, Sha512};

use alloc::vec::Vec;

#[derive(Debug)]
#[cfg_attr(feature = "std", derive(thiserror::Error))]
pub enum HashError {
    #[cfg_attr(feature = "std", error("invalid hmac length"))]
    InvalidHmacLength,
    #[cfg_attr(feature = "std", error("unsupported cipher suite"))]
    UnsupportedCipherSuite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Hash {
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    pub fn new(cipher_suite: CipherSuite) -> Result<Self, HashError> {
        match cipher_suite {
            CipherSuite::CURVE25519_AES128
            | CipherSuite::P256_AES128
            | CipherSuite::CURVE25519_CHACHA => Ok(Hash::Sha256),
            CipherSuite::P384_AES256 => Ok(Hash::Sha384),
            CipherSuite::CURVE448_AES256
            | CipherSuite::CURVE448_CHACHA
            | CipherSuite::P521_AES256 => Ok(Hash::Sha512),
            _ => Err(HashError::UnsupportedCipherSuite),
        }
    }

    pub fn hash(&self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha256 => Sha256::digest(data).to_vec(),
            Hash::Sha384 => Sha384::digest(data).to_vec(),
            Hash::Sha512 => Sha512::digest(data).to_vec(),
        }
    }

    pub fn mac(&self, key: &[u8], data: &[u8]) -> Result<Vec<u8>, HashError> {
        match self {
            Hash::Sha256 => generic_generate_tag(
                SimpleHmac::<Sha256>::new_from_slice(key)
                    .map_err(|_| HashError::InvalidHmacLength)?,
                data,
            ),
            Hash::Sha384 => generic_generate_tag(
                SimpleHmac::<Sha384>::new_from_slice(key)
                    .map_err(|_| HashError::InvalidHmacLength)?,
                data,
            ),
            Hash::Sha512 => generic_generate_tag(
                SimpleHmac::<Sha512>::new_from_slice(key)
                    .map_err(|_| HashError::InvalidHmacLength)?,
                data,
            ),
        }
    }
}

fn generic_generate_tag<D: Digest + BlockSizeUser + FixedOutputReset>(
    mut hmac: SimpleHmac<D>,
    data: &[u8],
) -> Result<Vec<u8>, HashError> {
    hmac.update(data);
    let res = hmac.finalize().into_bytes().to_vec();
    Ok(res)
}

#[cfg(test)]
mod test {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TestCase {
        pub ciphersuite: u16,
        #[serde(with = "hex::serde")]
        key: Vec<u8>,
        #[serde(with = "hex::serde")]
        message: Vec<u8>,
        #[serde(with = "hex::serde")]
        tag: Vec<u8>,
    }

    fn run_test_case(case: &TestCase) {
        // Test Sign
        let hash = Hash::new(case.ciphersuite.into()).unwrap();
        let tag = hash.mac(&case.key, &case.message).unwrap();
        assert_eq!(&tag, &case.tag);

        // Test different message
        let different_tag = hash.mac(&case.key, b"different message").unwrap();
        assert_ne!(&different_tag, &tag)
    }

    #[test]
    fn test_hmac_test_vectors() {
        let test_case_file = include_str!("../test_data/test_hmac.json");
        let test_cases: Vec<TestCase> = serde_json::from_str(test_case_file).unwrap();

        for case in test_cases {
            run_test_case(&case);
        }
    }
}
