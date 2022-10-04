use ferriscrypt::hpke::kem::HpkePublicKey;
use thiserror::Error;
use tls_codec_derive::{TlsDeserialize, TlsSerialize, TlsSize};

use super::{
    leaf_node::LeafNode,
    leaf_node_validator::{LeafNodeValidationError, LeafNodeValidator, ValidationContext},
    node::{LeafIndex, NodeVecError},
    tree_math::TreeMathError,
    HpkeCiphertext,
};
use crate::group::message_processor::ProvisionalState;
use crate::{extension::ExtensionError, provider::identity_validation::IdentityValidator};

#[derive(Clone, Debug, PartialEq, Eq, TlsDeserialize, TlsSerialize, TlsSize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct UpdatePathNode {
    #[tls_codec(with = "crate::tls::ByteVec")]
    pub public_key: HpkePublicKey,
    #[tls_codec(with = "crate::tls::DefVec")]
    pub encrypted_path_secret: Vec<HpkeCiphertext>,
}

#[derive(Clone, Debug, PartialEq, TlsDeserialize, TlsSerialize, TlsSize)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct UpdatePath {
    pub leaf_node: LeafNode,
    #[tls_codec(with = "crate::tls::DefVec")]
    pub nodes: Vec<UpdatePathNode>,
}

#[derive(Debug, Error)]
pub enum UpdatePathValidationError {
    #[error(transparent)]
    LeafNodeValidationError(#[from] LeafNodeValidationError),
    #[error("different identity in update for leaf {0:?}")]
    DifferentIdentity(LeafIndex),
    #[error("same HPKE leaf key before and after applying the update path for leaf {0:?}")]
    SameHpkeKey(LeafIndex),
    #[error(transparent)]
    CredentialValidationError(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    NodeVecError(#[from] NodeVecError),
    #[error(transparent)]
    ExtensionError(#[from] ExtensionError),
    #[error(transparent)]
    TreeMathError(#[from] TreeMathError),
    #[error("the length of the update path {0} different than the length of the direct path {1}")]
    WrongPathLen(usize, usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedUpdatePath {
    pub leaf_node: LeafNode,
    pub nodes: Vec<UpdatePathNode>,
}

pub(crate) fn validate_update_path<C: IdentityValidator>(
    identity_validator: &C,
    path: &UpdatePath,
    state: &ProvisionalState,
    sender: LeafIndex,
) -> Result<ValidatedUpdatePath, UpdatePathValidationError> {
    let required_capabilities = state.group_context.extensions.get_extension()?;

    let leaf_validator = LeafNodeValidator::new(
        state.group_context.cipher_suite,
        required_capabilities.as_ref(),
        identity_validator,
    );

    leaf_validator.check_if_valid(
        &path.leaf_node,
        ValidationContext::Commit(&state.group_context.group_id),
    )?;

    let existing_leaf = state.public_tree.nodes.borrow_as_leaf(sender)?;
    let original_leaf_node = existing_leaf.clone();

    let original_identity = identity_validator
        .identity(&original_leaf_node.signing_identity)
        .map_err(|e| UpdatePathValidationError::CredentialValidationError(e.into()))?;

    let updated_identity = identity_validator
        .identity(&path.leaf_node.signing_identity)
        .map_err(|e| UpdatePathValidationError::CredentialValidationError(e.into()))?;

    (state.external_init.is_some() || original_identity == updated_identity)
        .then_some(())
        .ok_or(UpdatePathValidationError::DifferentIdentity(sender))?;

    (state.external_init.is_some() || existing_leaf.public_key != path.leaf_node.public_key)
        .then_some(())
        .ok_or(UpdatePathValidationError::SameHpkeKey(sender))?;

    let path_copath = state
        .public_tree
        .nodes
        .filtered_direct_path_co_path(sender)?;

    (path.nodes.len() == path_copath.len())
        .then_some(())
        .ok_or(UpdatePathValidationError::WrongPathLen(
            path.nodes.len(),
            path_copath.len(),
        ))?;

    Ok(ValidatedUpdatePath {
        leaf_node: path.leaf_node.clone(),
        nodes: path.nodes.clone(),
    })
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;

    use ferriscrypt::{hpke::kem::HpkePublicKey, rand::SecureRng};

    use crate::group::message_processor::ProvisionalState;
    use crate::group::test_utils::get_test_group_context;
    use crate::provider::identity_validation::BasicIdentityValidator;
    use crate::tree_kem::leaf_node::test_utils::default_properties;
    use crate::tree_kem::node::LeafIndex;
    use crate::tree_kem::test_utils::{get_test_leaf_nodes, get_test_tree};
    use crate::tree_kem::validate_update_path;
    use crate::tree_kem::{
        leaf_node::test_utils::get_basic_test_node_sig_key, parent_hash::ParentHash,
    };

    use super::{UpdatePath, UpdatePathNode};
    use crate::{cipher_suite::CipherSuite, tree_kem::UpdatePathValidationError};

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    const TEST_GROUP_ID: &[u8] = &[];

    fn test_update_path(cipher_suite: CipherSuite, cred: &str) -> UpdatePath {
        let (mut leaf_node, _, signer) = get_basic_test_node_sig_key(cipher_suite, cred);

        leaf_node
            .commit(
                cipher_suite,
                TEST_GROUP_ID,
                default_properties(leaf_node.signing_identity.clone()),
                &signer,
                |_| Ok(ParentHash::empty()),
            )
            .unwrap();

        let ciphertext = ferriscrypt::hpke::HpkeCiphertext {
            enc: SecureRng::gen(32).unwrap(),
            ciphertext: SecureRng::gen(32).unwrap(),
        }
        .into();

        let node = UpdatePathNode {
            public_key: HpkePublicKey::from(SecureRng::gen(32).unwrap()),
            encrypted_path_secret: vec![ciphertext],
        };

        UpdatePath {
            leaf_node,
            nodes: vec![node.clone(), node],
        }
    }

    fn test_provisional_state(cipher_suite: CipherSuite) -> ProvisionalState {
        let mut tree = get_test_tree(cipher_suite).public;
        let leaf_nodes = get_test_leaf_nodes(cipher_suite);

        tree.add_leaves(leaf_nodes, BasicIdentityValidator::new())
            .unwrap();

        ProvisionalState {
            public_tree: tree,
            added_leaves: vec![],
            removed_leaves: vec![],
            updated_leaves: vec![],
            group_context: get_test_group_context(1, cipher_suite),
            epoch: 1,
            path_update_required: true,
            psks: vec![],
            reinit: None,
            external_init: None,
            rejected_proposals: vec![],
        }
    }

    #[test]
    fn test_valid_leaf_node() {
        let cipher_suite = CipherSuite::Curve25519Aes128;
        let update_path = test_update_path(cipher_suite, "creator");

        let validated = validate_update_path(
            &BasicIdentityValidator::new(),
            &update_path,
            &test_provisional_state(cipher_suite),
            LeafIndex(0),
        )
        .unwrap();

        assert_eq!(validated.nodes, update_path.nodes);
        assert_eq!(validated.leaf_node, update_path.leaf_node);
    }

    #[test]
    fn test_invalid_key_package() {
        let cipher_suite = CipherSuite::Curve25519Aes128;
        let mut update_path = test_update_path(cipher_suite, "creator");
        update_path.leaf_node.signature = SecureRng::gen(32).unwrap();

        let validated = validate_update_path(
            &BasicIdentityValidator::new(),
            &update_path,
            &test_provisional_state(cipher_suite),
            LeafIndex(0),
        );

        assert_matches!(
            validated,
            Err(UpdatePathValidationError::LeafNodeValidationError(_))
        );
    }

    #[test]
    fn validating_path_fails_with_different_identity() {
        let cipher_suite = CipherSuite::Curve25519Aes128;
        let update_path = test_update_path(cipher_suite, "foobar");

        let validated = validate_update_path(
            &BasicIdentityValidator::new(),
            &update_path,
            &test_provisional_state(cipher_suite),
            LeafIndex(0),
        );

        assert_matches!(
            validated,
            Err(UpdatePathValidationError::DifferentIdentity(_))
        );
    }

    #[test]
    fn validating_path_fails_with_same_hpke_key() {
        let cipher_suite = CipherSuite::Curve25519Aes128;
        let update_path = test_update_path(cipher_suite, "creator");
        let mut state = test_provisional_state(cipher_suite);

        state
            .public_tree
            .nodes
            .borrow_as_leaf_mut(LeafIndex(0))
            .unwrap()
            .public_key = update_path.leaf_node.public_key.clone();

        let validated = validate_update_path(
            &BasicIdentityValidator::new(),
            &update_path,
            &state,
            LeafIndex(0),
        );

        assert_matches!(validated, Err(UpdatePathValidationError::SameHpkeKey(_)));
    }
}
