use assert_matches::assert_matches;
use aws_mls::client_builder::{ClientBuilder, MlsConfig, Preferences};
use aws_mls::error::MlsError;
use aws_mls::group::proposal::Proposal;
use aws_mls::group::ReceivedMessage;
use aws_mls::identity::basic::BasicIdentityProvider;
use aws_mls::identity::SigningIdentity;
use aws_mls::storage_provider::in_memory::InMemoryKeychainStorage;
use aws_mls::ExtensionList;
use aws_mls::ProtocolVersion;
use aws_mls::{CipherSuite, Group};
use aws_mls::{Client, CryptoProvider};
use aws_mls_core::crypto::CipherSuiteProvider;
use cfg_if::cfg_if;
use rand::prelude::SliceRandom;
use rand::RngCore;

use aws_mls::test_utils::{all_process_message, get_test_basic_credential, TestClient};

#[cfg(not(sync))]
use futures::Future;

cfg_if! {
    if #[cfg(target_arch = "wasm32")] {
        pub use aws_mls_crypto_rustcrypto::RustCryptoProvider as TestCryptoProvider;
    } else {
        pub use aws_mls_crypto_openssl::OpensslCryptoProvider as TestCryptoProvider;
    }
}

fn generate_client(
    cipher_suite: CipherSuite,
    id: usize,
    preferences: &Preferences,
) -> TestClient<impl MlsConfig> {
    aws_mls::test_utils::generate_basic_client(
        cipher_suite,
        id,
        preferences,
        &TestCryptoProvider::default(),
    )
}

#[maybe_async::maybe_async]
pub async fn get_test_groups(
    version: ProtocolVersion,
    cipher_suite: CipherSuite,
    num_participants: usize,
    preferences: &Preferences,
) -> Vec<Group<impl MlsConfig>> {
    aws_mls::test_utils::get_test_groups(
        version,
        cipher_suite,
        num_participants,
        preferences,
        &TestCryptoProvider::default(),
    )
    .await
}

use rand::seq::IteratorRandom;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};

#[cfg(target_arch = "wasm32")]
wasm_bindgen_test_configure!(run_in_browser);

#[cfg(feature = "private_message")]
#[maybe_async::async_impl]
async fn test_on_all_params<F, Fut>(test: F)
where
    F: Fn(ProtocolVersion, CipherSuite, usize, Preferences) -> Fut,
    Fut: Future<Output = ()>,
{
    for version in ProtocolVersion::all() {
        for cs in TestCryptoProvider::all_supported_cipher_suites() {
            for encrypt_controls in [true, false] {
                let preferences = Preferences::default().with_control_encryption(encrypt_controls);

                test(version, cs, 10, preferences).await;
            }
        }
    }
}

#[cfg(feature = "private_message")]
#[maybe_async::sync_impl]
fn test_on_all_params<F>(test: F)
where
    F: Fn(ProtocolVersion, CipherSuite, usize, Preferences),
{
    for version in ProtocolVersion::all() {
        for cs in TestCryptoProvider::all_supported_cipher_suites() {
            for encrypt_controls in [true, false] {
                let preferences = Preferences::default().with_control_encryption(encrypt_controls);

                test(version, cs, 10, preferences);
            }
        }
    }
}

#[cfg(not(feature = "private_message"))]
#[maybe_async::async_impl]
async fn test_on_all_params<F, Fut>(test: F)
where
    F: Fn(ProtocolVersion, CipherSuite, usize, Preferences) -> Fut,
    Fut: Future<Output = ()>,
{
    test_on_all_params_plaintext(test).await;
}

#[maybe_async::async_impl]
async fn test_on_all_params_plaintext<F, Fut>(test: F)
where
    F: Fn(ProtocolVersion, CipherSuite, usize, Preferences) -> Fut,
    Fut: Future<Output = ()>,
{
    for version in ProtocolVersion::all() {
        for cs in TestCryptoProvider::all_supported_cipher_suites() {
            test(version, cs, 10, Preferences::default()).await;
        }
    }
}

#[maybe_async::sync_impl]
fn test_on_all_params_plaintext<F>(test: F)
where
    F: Fn(ProtocolVersion, CipherSuite, usize, Preferences),
{
    for version in ProtocolVersion::all() {
        for cs in TestCryptoProvider::all_supported_cipher_suites() {
            test(version, cs, 10, Preferences::default());
        }
    }
}

#[maybe_async::maybe_async]
async fn test_create(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    _n_participants: usize,
    preferences: Preferences,
) {
    println!(
        "Testing group creation for cipher suite: {protocol_version:?} {cipher_suite:?}, participants: 1, {preferences:?}"
    );

    let alice = generate_client(cipher_suite, 0, &preferences);
    let bob = generate_client(cipher_suite, 1, &preferences);

    let bob_key_pkg = bob
        .client
        .generate_key_package_message(protocol_version, cipher_suite, bob.identity)
        .await
        .unwrap();

    // Alice creates a group and adds bob
    let mut alice_group = alice
        .client
        .create_group_with_id(
            protocol_version,
            cipher_suite,
            b"group".to_vec(),
            alice.identity,
            ExtensionList::default(),
        )
        .await
        .unwrap();

    let welcome = alice_group
        .commit_builder()
        .add_member(bob_key_pkg)
        .unwrap()
        .build()
        .await
        .unwrap()
        .welcome_message;

    // Upon server confirmation, alice applies the commit to her own state
    alice_group.apply_pending_commit().await.unwrap();

    let tree = alice_group.export_tree().unwrap();

    // Bob receives the welcome message and joins the group
    let (bob_group, _) = bob
        .client
        .join_group(Some(&tree), welcome.unwrap())
        .await
        .unwrap();

    assert!(Group::equal_group_state(&alice_group, &bob_group));
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_create_group() {
    test_on_all_params(test_create).await;
}

#[maybe_async::maybe_async]
async fn test_empty_commits(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    participants: usize,
    preferences: Preferences,
) {
    println!(
        "Testing empty commits for cipher suite: {cipher_suite:?}, participants: {participants}, {preferences:?}",
    );

    let mut groups =
        get_test_groups(protocol_version, cipher_suite, participants, &preferences).await;

    // Loop through each participant and send a path update

    for i in 0..groups.len() {
        // Create the commit
        let commit_output = groups[i].commit(Vec::new()).await.unwrap();

        assert!(commit_output.welcome_message.is_none());

        let index = groups[i].current_member_index() as usize;
        all_process_message(&mut groups, &commit_output.commit_message, index, true).await;

        for other_group in groups.iter() {
            assert!(Group::equal_group_state(other_group, &groups[i]));
        }
    }
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_group_path_updates() {
    test_on_all_params(test_empty_commits).await;
}

#[cfg(feature = "by_ref_proposal")]
#[maybe_async::maybe_async]
async fn test_update_proposals(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    participants: usize,
    preferences: Preferences,
) {
    println!(
        "Testing update proposals for cipher suite: {cipher_suite:?}, participants: {participants}, {preferences:?}",
    );

    let mut groups =
        get_test_groups(protocol_version, cipher_suite, participants, &preferences).await;

    // Create an update from the ith member, have the ith + 1 member commit it
    for i in 0..groups.len() - 1 {
        let update_proposal_msg = groups[i].propose_update(Vec::new()).await.unwrap();

        let sender = groups[i].current_member_index() as usize;
        all_process_message(&mut groups, &update_proposal_msg, sender, false).await;

        // Everyone receives the commit
        let committer_index = i + 1;

        let commit_output = groups[committer_index].commit(Vec::new()).await.unwrap();

        assert!(commit_output.welcome_message.is_none());

        let commit = commit_output.commit_message();

        all_process_message(&mut groups, commit, committer_index, true).await;

        groups
            .iter()
            .for_each(|g| assert!(Group::equal_group_state(g, &groups[0])));
    }
}

#[cfg(feature = "by_ref_proposal")]
#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_group_update_proposals() {
    test_on_all_params(test_update_proposals).await;
}

#[maybe_async::maybe_async]
async fn test_remove_proposals(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    participants: usize,
    preferences: Preferences,
) {
    println!(
        "Testing remove proposals for cipher suite: {cipher_suite:?}, participants: {participants}, {preferences:?}",
    );

    let mut groups =
        get_test_groups(protocol_version, cipher_suite, participants, &preferences).await;

    // Remove people from the group one at a time
    while groups.len() > 1 {
        let removed_and_committer = (0..groups.len()).choose_multiple(&mut rand::thread_rng(), 2);

        let to_remove = removed_and_committer[0];
        let committer = removed_and_committer[1];
        let to_remove_index = groups[to_remove].current_member_index();

        let epoch_before_remove = groups[committer].current_epoch();

        let commit_output = groups[committer]
            .commit_builder()
            .remove_member(to_remove_index)
            .unwrap()
            .build()
            .await
            .unwrap();

        assert!(commit_output.welcome_message.is_none());

        let commit = commit_output.commit_message();
        let committer_index = groups[committer].current_member_index() as usize;
        all_process_message(&mut groups, commit, committer_index, true).await;

        // Check that remove was effective
        for (i, group) in groups.iter().enumerate() {
            if i == to_remove {
                assert_eq!(group.current_epoch(), epoch_before_remove);
            } else {
                assert_eq!(group.current_epoch(), epoch_before_remove + 1);

                assert!(group
                    .roster()
                    .iter()
                    .all(|member| member.index() != to_remove_index));
            }
        }

        groups.retain(|group| group.current_member_index() != to_remove_index);

        for one_group in groups.iter() {
            assert!(Group::equal_group_state(one_group, &groups[0]))
        }
    }
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_group_remove_proposals() {
    test_on_all_params(test_remove_proposals).await;
}

#[cfg(feature = "private_message")]
#[maybe_async::maybe_async]
async fn test_application_messages(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    participants: usize,
    preferences: Preferences,
) {
    let message_count = 20;

    let mut groups =
        get_test_groups(protocol_version, cipher_suite, participants, &preferences).await;

    // Loop through each participant and send application messages
    for i in 0..groups.len() {
        let mut test_message = vec![0; 1024];
        rand::thread_rng().fill_bytes(&mut test_message);

        for _ in 0..message_count {
            // Encrypt the application message
            let ciphertext = groups[i]
                .encrypt_application_message(&test_message, Vec::new())
                .await
                .unwrap();

            let sender_index = groups[i].current_member_index();

            for g in groups.iter_mut() {
                if g.current_member_index() != sender_index {
                    let decrypted = g
                        .process_incoming_message(ciphertext.clone())
                        .await
                        .unwrap();

                    assert_matches!(decrypted, ReceivedMessage::ApplicationMessage(m) if m.data() == test_message);
                }
            }
        }
    }
}

#[cfg(all(feature = "private_message", feature = "out_of_order"))]
#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_out_of_order_application_messages() {
    let mut groups = get_test_groups(
        ProtocolVersion::MLS_10,
        CipherSuite::CURVE25519_AES128,
        2,
        &Preferences::default(),
    )
    .await;

    let mut alice_group = groups[0].clone();
    let bob_group = &mut groups[1];

    let ciphertext = alice_group
        .encrypt_application_message(&[0], Vec::new())
        .await
        .unwrap();

    let mut ciphertexts = vec![ciphertext];

    ciphertexts.push(
        alice_group
            .encrypt_application_message(&[1], Vec::new())
            .await
            .unwrap(),
    );

    let commit = alice_group.commit(Vec::new()).await.unwrap().commit_message;

    alice_group.apply_pending_commit().await.unwrap();

    bob_group.process_incoming_message(commit).await.unwrap();

    ciphertexts.push(
        alice_group
            .encrypt_application_message(&[2], Vec::new())
            .await
            .unwrap(),
    );

    ciphertexts.push(
        alice_group
            .encrypt_application_message(&[3], Vec::new())
            .await
            .unwrap(),
    );

    for i in [3, 2, 1, 0] {
        let res = bob_group
            .process_incoming_message(ciphertexts[i].clone())
            .await
            .unwrap();

        assert_matches!(
            res,
            ReceivedMessage::ApplicationMessage(m) if m.data() == [i as u8]
        );
    }
}

#[cfg(feature = "private_message")]
#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_group_application_messages() {
    test_on_all_params(test_application_messages).await
}

#[maybe_async::maybe_async]
async fn processing_message_from_self_returns_error(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    _n_participants: usize,
    preferences: Preferences,
) {
    println!(
        "Verifying that processing one's own message returns an error for cipher suite: {cipher_suite:?}, {preferences:?}",
    );

    let mut creator_group = get_test_groups(protocol_version, cipher_suite, 1, &preferences).await;
    let creator_group = &mut creator_group[0];

    let commit = creator_group
        .commit(Vec::new())
        .await
        .unwrap()
        .commit_message;

    let error = creator_group
        .process_incoming_message(commit)
        .await
        .unwrap_err();

    assert_matches!(error, MlsError::CantProcessMessageFromSelf);
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_processing_message_from_self_returns_error() {
    test_on_all_params(processing_message_from_self_returns_error).await;
}

#[cfg(feature = "external_commit")]
#[maybe_async::maybe_async]
async fn external_commits_work(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    _n_participants: usize,
    preferences: Preferences,
) {
    let creator = generate_client(cipher_suite, 0, &preferences);

    let creator_group = creator
        .client
        .create_group_with_id(
            protocol_version,
            cipher_suite,
            b"group".to_vec(),
            creator.identity,
            ExtensionList::default(),
        )
        .await
        .unwrap();

    const PARTICIPANT_COUNT: usize = 10;

    let others = (1..PARTICIPANT_COUNT)
        .map(|i| generate_client(cipher_suite, i, &Default::default()))
        .collect::<Vec<_>>();

    let mut groups = vec![creator_group];

    for client in &others {
        let existing_group = groups.choose_mut(&mut rand::thread_rng()).unwrap();

        let group_info = existing_group
            .group_info_message_allowing_ext_commit()
            .await
            .unwrap();

        let (new_group, commit) = client
            .client
            .external_commit_builder(client.identity.clone())
            .with_tree_data(existing_group.export_tree().unwrap())
            .build(group_info)
            .await
            .unwrap();

        for group in groups.iter_mut() {
            group
                .process_incoming_message(commit.clone())
                .await
                .unwrap();
        }

        groups.push(new_group);
    }

    assert!(groups
        .iter()
        .all(|group| group.roster().len() == PARTICIPANT_COUNT));

    for i in 0..groups.len() {
        let message = groups[i].propose_remove(0, Vec::new()).await.unwrap();

        for (_, group) in groups.iter_mut().enumerate().filter(|&(j, _)| i != j) {
            let processed = group
                .process_incoming_message(message.clone())
                .await
                .unwrap();

            if let ReceivedMessage::Proposal(p) = &processed {
                if let Proposal::Remove(r) = &p.proposal {
                    if r.to_remove() == 0 {
                        continue;
                    }
                }
            }

            panic!("expected a proposal, got {processed:?}");
        }
    }
}

#[cfg(feature = "external_commit")]
#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_external_commits() {
    test_on_all_params_plaintext(external_commits_work).await
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn test_remove_nonexisting_leaf() {
    let mut groups = get_test_groups(
        ProtocolVersion::MLS_10,
        CipherSuite::CURVE25519_AES128,
        10,
        &Preferences::default(),
    )
    .await;

    groups[0]
        .commit_builder()
        .remove_member(5)
        .unwrap()
        .build()
        .await
        .unwrap();
    groups[0].apply_pending_commit().await.unwrap();

    // Leaf index out of bounds
    assert!(groups[0].commit_builder().remove_member(13).is_err());

    // Removing blank leaf causes error
    assert!(groups[0].commit_builder().remove_member(5).is_err());
}

#[cfg(feature = "psk")]
struct ReinitClientGeneration<C: MlsConfig> {
    client: Client<C>,
    id1: SigningIdentity,
    id2: SigningIdentity,
}

#[cfg(feature = "psk")]
fn get_reinit_client(
    suite1: CipherSuite,
    suite2: CipherSuite,
    id: &str,
) -> ReinitClientGeneration<impl MlsConfig> {
    let credential = get_test_basic_credential(id.as_bytes().to_vec());

    let csp1 = TestCryptoProvider::new()
        .cipher_suite_provider(suite1)
        .unwrap();

    let csp2 = TestCryptoProvider::new()
        .cipher_suite_provider(suite2)
        .unwrap();

    let (sk1, pk1) = csp1.signature_key_generate().unwrap();
    let (sk2, pk2) = csp2.signature_key_generate().unwrap();

    let id1 = SigningIdentity::new(credential.clone(), pk1);
    let id2 = SigningIdentity::new(credential, pk2);

    let client = ClientBuilder::new()
        .crypto_provider(TestCryptoProvider::default())
        .identity_provider(BasicIdentityProvider::new())
        .keychain(InMemoryKeychainStorage::default())
        .signing_identity(id1.clone(), sk1, suite1)
        .signing_identity(id2.clone(), sk2, suite2)
        .build();

    ReinitClientGeneration { client, id1, id2 }
}

#[cfg(feature = "psk")]
#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn reinit_works() {
    let suite1 = CipherSuite::CURVE25519_AES128;
    let suite2 = CipherSuite::P256_AES128;
    let version = ProtocolVersion::MLS_10;

    // Create a group with 2 parties
    let alice = get_reinit_client(suite1, suite2, "alice");
    let bob = get_reinit_client(suite1, suite2, "bob");

    let mut alice_group = alice
        .client
        .create_group(version, suite1, alice.id1.clone(), ExtensionList::new())
        .await
        .unwrap();

    let kp = bob
        .client
        .generate_key_package_message(version, suite1, bob.id1)
        .await
        .unwrap();

    let welcome = alice_group
        .commit_builder()
        .add_member(kp)
        .unwrap()
        .build()
        .await
        .unwrap()
        .welcome_message;

    alice_group.apply_pending_commit().await.unwrap();
    let tree = alice_group.export_tree().unwrap();

    let (mut bob_group, _) = bob
        .client
        .join_group(Some(&tree), welcome.unwrap())
        .await
        .unwrap();

    // Alice proposes reinit
    let reinit_proposal_message = alice_group
        .propose_reinit(
            None,
            ProtocolVersion::MLS_10,
            suite2,
            ExtensionList::default(),
            Vec::new(),
        )
        .await
        .unwrap();

    // Bob commits the reinit
    bob_group
        .process_incoming_message(reinit_proposal_message)
        .await
        .unwrap();

    let commit = bob_group.commit(Vec::new()).await.unwrap().commit_message;

    // Both process Bob's commit

    #[cfg(feature = "state_update")]
    {
        let state_update = bob_group.apply_pending_commit().await.unwrap().state_update;
        assert!(!state_update.is_active() && state_update.is_pending_reinit());
    }

    #[cfg(not(feature = "state_update"))]
    bob_group.apply_pending_commit().await.unwrap();

    let message = alice_group.process_incoming_message(commit).await.unwrap();

    #[cfg(feature = "state_update")]
    if let ReceivedMessage::Commit(commit_description) = message {
        assert!(
            !commit_description.state_update.is_active()
                && commit_description.state_update.is_pending_reinit()
        );
    }

    #[cfg(not(feature = "state_update"))]
    assert_matches!(message, ReceivedMessage::Commit(_));

    // They can't create new epochs anymore
    let res = alice_group.commit(Vec::new()).await;

    assert!(res.is_err());

    let res = bob_group.commit(Vec::new()).await;

    assert!(res.is_err());

    // Alice finishes the reinit by creating the new group
    let kp = bob
        .client
        .generate_key_package_message(version, suite2, bob.id2)
        .await
        .unwrap();

    let (mut alice_group, welcome) = alice_group
        .finish_reinit_commit(vec![kp], Some(alice.id2), None)
        .await
        .unwrap();

    // Alice invited Bob
    let welcome = welcome.unwrap();
    let tree = alice_group.export_tree().unwrap();

    let (mut bob_group, _) = bob_group
        .finish_reinit_join(welcome, Some(&tree))
        .await
        .unwrap();

    // They can talk
    let carol = get_reinit_client(suite1, suite2, "carol");

    let kp = carol
        .client
        .generate_key_package_message(version, suite2, carol.id2)
        .await
        .unwrap();

    let commit_output = alice_group
        .commit_builder()
        .add_member(kp)
        .unwrap()
        .build()
        .await
        .unwrap();

    alice_group.apply_pending_commit().await.unwrap();

    bob_group
        .process_incoming_message(commit_output.commit_message)
        .await
        .unwrap();

    let tree = alice_group.export_tree().unwrap();

    carol
        .client
        .join_group(Some(&tree), commit_output.welcome_message.unwrap())
        .await
        .unwrap();
}

#[cfg(feature = "external_commit")]
#[futures_test::test]
async fn external_joiner_can_process_siblings_update() {
    let mut groups = get_test_groups(
        ProtocolVersion::MLS_10,
        CipherSuite::P256_AES128,
        3,
        &Preferences::default().with_ratchet_tree_extension(true),
    )
    .await;

    // Remove leaf 1 s.t. the external joiner joins in its place
    let c = groups[0]
        .commit_builder()
        .remove_member(1)
        .unwrap()
        .build()
        .await
        .unwrap();

    all_process_message(&mut groups, &c.commit_message, 0, true).await;

    let info = groups[0]
        .group_info_message_allowing_ext_commit()
        .await
        .unwrap();

    // Create the external joiner and join
    let new_client = generate_client(CipherSuite::P256_AES128, 0xabba, &Preferences::default());

    let (mut group, commit) = new_client
        .client
        .commit_external(info, new_client.identity)
        .await
        .unwrap();

    all_process_message(&mut groups, &commit, 1, false).await;
    groups.remove(1);

    // New client's sibling proposes an update to blank their common parent
    let p = groups[0].propose_update(Vec::new()).await.unwrap();
    all_process_message(&mut groups, &p, 0, false).await;
    group.process_incoming_message(p).await.unwrap();

    // Some other member commits
    let c = groups[1].commit(Vec::new()).await.unwrap().commit_message;
    all_process_message(&mut groups, &c, 2, true).await;
    group.process_incoming_message(c).await.unwrap();
}
