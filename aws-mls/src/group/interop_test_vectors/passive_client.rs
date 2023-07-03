use alloc::vec;
use alloc::vec::Vec;

use aws_mls_core::{
    crypto::{CipherSuite, CipherSuiteProvider, CryptoProvider},
    identity::SigningIdentity,
    protocol_version::ProtocolVersion,
    psk::ExternalPskId,
};
use itertools::Itertools;
use rand::{seq::IteratorRandom, Rng, SeedableRng};

use crate::{
    client_builder::{ClientBuilder, MlsConfig},
    crypto::test_utils::TestCryptoProvider,
    group::{
        internal::{ClientConfig, Preferences},
        CommitBuilder,
    },
    identity::basic::BasicIdentityProvider,
    key_package::KeyPackageGeneration,
    storage_provider::in_memory::InMemoryKeyPackageStorage,
    test_utils::{
        all_process_message, generate_basic_client, get_test_basic_credential, get_test_groups,
        make_test_ext_psk, TestClient, TEST_EXT_PSK_ID,
    },
    Group, MLSMessage,
};

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct TestCase {
    pub cipher_suite: u16,

    pub external_psks: Vec<TestExternalPsk>,
    #[serde(with = "hex::serde")]
    pub key_package: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub signature_priv: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub encryption_priv: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub init_priv: Vec<u8>,

    #[serde(with = "hex::serde")]
    pub welcome: Vec<u8>,
    pub ratchet_tree: Option<TestRatchetTree>,
    #[serde(with = "hex::serde")]
    pub initial_epoch_authenticator: Vec<u8>,

    pub epochs: Vec<TestEpoch>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct TestExternalPsk {
    #[serde(with = "hex::serde")]
    pub psk_id: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub psk: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct TestEpoch {
    pub proposals: Vec<TestMLSMessage>,
    #[serde(with = "hex::serde")]
    pub commit: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub epoch_authenticator: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct TestMLSMessage(#[serde(with = "hex::serde")] pub Vec<u8>);

#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct TestRatchetTree(#[serde(with = "hex::serde")] pub Vec<u8>);

impl TestEpoch {
    pub fn new(
        proposals: Vec<MLSMessage>,
        commit: &MLSMessage,
        epoch_authenticator: Vec<u8>,
    ) -> Self {
        let proposals = proposals
            .into_iter()
            .map(|p| TestMLSMessage(p.to_bytes().unwrap()))
            .collect();

        Self {
            proposals,
            commit: commit.to_bytes().unwrap(),
            epoch_authenticator,
        }
    }
}

#[maybe_async::test(sync, async(not(sync), futures_test::test))]
async fn interop_passive_client() {
    // Test vectors can be found here:
    // * https://github.com/mlswg/mls-implementations/blob/main/test-vectors/passive-client-welcome.json
    // * https://github.com/mlswg/mls-implementations/blob/main/test-vectors/passive-client-handle-commit.json
    // * https://github.com/mlswg/mls-implementations/blob/main/test-vectors/passive-client-random.json

    #[cfg(not(sync))]
    let (test_cases_wel, test_cases_com, test_cases_rand) = {
        let test_cases_wel: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_welcome,
            generate_passive_client_welcome_tests().await
        );

        let test_cases_com: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_handle_commit,
            generate_passive_client_proposal_tests().await
        );

        let test_cases_rand: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_random,
            generate_passive_client_random_tests().await
        );

        (test_cases_wel, test_cases_com, test_cases_rand)
    };

    #[cfg(sync)]
    let (test_cases_wel, test_cases_com, test_cases_rand) = {
        let test_cases_wel: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_welcome,
            generate_passive_client_welcome_tests()
        );

        let test_cases_com: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_handle_commit,
            generate_passive_client_proposal_tests()
        );

        let test_cases_rand: Vec<TestCase> = load_test_case_json!(
            interop_passive_client_random,
            generate_passive_client_random_tests()
        );

        (test_cases_wel, test_cases_com, test_cases_rand)
    };

    for test_case in vec![]
        .into_iter()
        .chain(test_cases_com.into_iter())
        .chain(test_cases_wel.into_iter())
        .chain(test_cases_rand.into_iter())
    {
        let crypto_provider = TestCryptoProvider::new();
        let Some(cs) = crypto_provider.cipher_suite_provider(test_case.cipher_suite.into()) else { continue };

        let message = MLSMessage::from_bytes(&test_case.key_package).unwrap();
        let key_package = message.into_key_package().unwrap();
        let id = key_package.leaf_node.signing_identity.clone();
        let key = test_case.signature_priv.clone().into();

        let mut client_builder = ClientBuilder::new()
            .crypto_provider(crypto_provider)
            .identity_provider(BasicIdentityProvider::new())
            .single_signing_identity(id, key, cs.cipher_suite());

        for psk in test_case.external_psks {
            client_builder = client_builder.psk(ExternalPskId::new(psk.psk_id), psk.psk.into());
        }

        let client = client_builder.build();

        let key_pckg_gen = KeyPackageGeneration {
            reference: key_package.to_reference(&cs).unwrap(),
            key_package,
            init_secret_key: test_case.init_priv.into(),
            leaf_node_secret_key: test_case.encryption_priv.into(),
        };

        let (id, pkg) = key_pckg_gen.to_storage().unwrap();
        client.config.key_package_repo().insert(id, pkg);

        let welcome = MLSMessage::from_bytes(&test_case.welcome).unwrap();
        let tree = test_case.ratchet_tree.as_ref().map(|t| t.0.as_slice());

        let (mut group, _info) = client.join_group(tree, welcome).await.unwrap();

        assert_eq!(
            group.epoch_authenticator().unwrap().to_vec(),
            test_case.initial_epoch_authenticator
        );

        for epoch in test_case.epochs {
            for proposal in epoch.proposals.iter() {
                let message = MLSMessage::from_bytes(&proposal.0).unwrap();
                group.process_incoming_message(message).await.unwrap();
            }

            let message = MLSMessage::from_bytes(&epoch.commit).unwrap();
            group.process_incoming_message(message).await.unwrap();

            assert_eq!(
                epoch.epoch_authenticator,
                group.epoch_authenticator().unwrap().to_vec()
            );
        }
    }
}

#[maybe_async::maybe_async]
async fn invite_passive_client<P: CipherSuiteProvider>(
    groups: &mut [Group<impl MlsConfig>],
    with_psk: bool,
    cs: &P,
) -> TestCase {
    let crypto_provider = TestCryptoProvider::new();

    let (secret_key, public_key) = cs.signature_key_generate().unwrap();
    let credential = get_test_basic_credential(b"Arnold".to_vec());
    let identity = SigningIdentity::new(credential, public_key);
    let key_package_repo = InMemoryKeyPackageStorage::new();

    let client = ClientBuilder::new()
        .crypto_provider(crypto_provider)
        .identity_provider(BasicIdentityProvider::new())
        .single_signing_identity(identity.clone(), secret_key.clone(), cs.cipher_suite())
        .key_package_repo(key_package_repo.clone())
        .build();

    let key_pckg = client
        .generate_key_package_message(ProtocolVersion::MLS_10, cs.cipher_suite(), identity)
        .await
        .unwrap();

    let (_, key_pckg_secrets) = key_package_repo.key_packages()[0].clone();

    let mut commit_builder = groups[0]
        .commit_builder()
        .add_member(key_pckg.clone())
        .unwrap();

    if with_psk {
        commit_builder = commit_builder
            .add_external_psk(ExternalPskId::new(TEST_EXT_PSK_ID.to_vec()))
            .unwrap();
    }

    let commit = commit_builder.build().await.unwrap();

    all_process_message(groups, &commit.commit_message, 0, true).await;

    let external_psk = TestExternalPsk {
        psk_id: TEST_EXT_PSK_ID.to_vec(),
        psk: make_test_ext_psk(),
    };

    TestCase {
        cipher_suite: cs.cipher_suite().into(),
        key_package: key_pckg.to_bytes().unwrap(),
        encryption_priv: key_pckg_secrets.leaf_node_key.to_vec(),
        init_priv: key_pckg_secrets.init_key.to_vec(),
        welcome: commit.welcome_message.unwrap().to_bytes().unwrap(),
        initial_epoch_authenticator: groups[0].epoch_authenticator().unwrap().to_vec(),
        epochs: vec![],
        signature_priv: secret_key.to_vec(),
        external_psks: if with_psk { vec![external_psk] } else { vec![] },
        ratchet_tree: None,
    }
}

#[maybe_async::maybe_async]
pub async fn generate_passive_client_proposal_tests() {
    let mut test_cases: Vec<TestCase> = vec![];

    for cs in CipherSuite::all() {
        let crypto_provider = TestCryptoProvider::new();
        let Some(cs) = crypto_provider.cipher_suite_provider(cs) else { continue };

        let mut groups = get_test_groups(
            ProtocolVersion::MLS_10,
            cs.cipher_suite(),
            7,
            &Preferences::default().with_ratchet_tree_extension(true),
            &crypto_provider,
        )
        .await;

        let mut partial_test_case = invite_passive_client(&mut groups, false, &cs).await;

        // Create a new epoch s.t. the passive member can process resumption PSK from the current one
        let commit = groups[0].commit(vec![]).await.unwrap();
        all_process_message(&mut groups, &commit.commit_message, 0, true).await;

        partial_test_case.epochs.push(TestEpoch::new(
            vec![],
            &commit.commit_message,
            groups[0].epoch_authenticator().unwrap().to_vec(),
        ));

        let psk = ExternalPskId::new(TEST_EXT_PSK_ID.to_vec());
        let key_pckg = create_key_package(cs.cipher_suite()).await;

        // Create by value proposals
        let test_case = commit_by_value(
            &mut groups[3].clone(),
            |b| b.add_member(key_pckg.clone()).unwrap(),
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        let test_case = commit_by_value(
            &mut groups[3].clone(),
            |b| b.remove_member(5).unwrap(),
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        let test_case = commit_by_value(
            &mut groups[1].clone(),
            |b| b.add_external_psk(psk.clone()).unwrap(),
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        let test_case = commit_by_value(
            &mut groups[5].clone(),
            |b| b.add_resumption_psk(groups[1].current_epoch() - 1).unwrap(),
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        let test_case = commit_by_value(
            &mut groups[2].clone(),
            |b| b.set_group_context_ext(Default::default()).unwrap(),
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        let test_case = commit_by_value(
            &mut groups[3].clone(),
            |b| {
                b.add_member(key_pckg)
                    .unwrap()
                    .remove_member(5)
                    .unwrap()
                    .add_external_psk(psk.clone())
                    .unwrap()
                    .add_resumption_psk(groups[4].current_epoch() - 1)
                    .unwrap()
                    .set_group_context_ext(Default::default())
                    .unwrap()
            },
            partial_test_case.clone(),
        )
        .await;

        test_cases.push(test_case);

        // Create by reference proposals
        let add = groups[0]
            .propose_add(create_key_package(cs.cipher_suite()).await, vec![])
            .await
            .unwrap();

        let add = (add, 0);

        let update = (groups[1].propose_update(vec![]).await.unwrap(), 1);
        let remove = (groups[2].propose_remove(2, vec![]).await.unwrap(), 2);

        let ext_psk = groups[3]
            .propose_external_psk(psk.clone(), vec![])
            .await
            .unwrap();

        let ext_psk = (ext_psk, 3);

        let last_ep = groups[3].current_epoch() - 1;

        let res_psk = groups[3]
            .propose_resumption_psk(last_ep, vec![])
            .await
            .unwrap();

        let res_psk = (res_psk, 3);

        let grp_ext = groups[4]
            .propose_group_context_extensions(Default::default(), vec![])
            .await
            .unwrap();

        let grp_ext = (grp_ext, 4);

        let proposals = [add, update, remove, ext_psk, res_psk, grp_ext];

        for (p, sender) in &proposals {
            let mut groups = groups.clone();

            all_process_message(&mut groups, p, *sender, false).await;

            let commit = groups[5].commit(vec![]).await.unwrap().commit_message;

            groups[5].apply_pending_commit().await.unwrap();
            let auth = groups[5].epoch_authenticator().unwrap().to_vec();

            let mut test_case = partial_test_case.clone();
            let epoch = TestEpoch::new(vec![p.clone()], &commit, auth);
            test_case.epochs.push(epoch);

            test_cases.push(test_case);
        }

        let mut group = groups[4].clone();

        for (p, _) in proposals.iter().filter(|(_, i)| *i != 4) {
            group.process_incoming_message(p.clone()).await.unwrap();
        }

        let commit = group.commit(vec![]).await.unwrap().commit_message;
        group.apply_pending_commit().await.unwrap();
        let auth = group.epoch_authenticator().unwrap().to_vec();
        let mut test_case = partial_test_case.clone();
        let proposals = proposals.into_iter().map(|(p, _)| p).collect();
        let epoch = TestEpoch::new(proposals, &commit, auth);
        test_case.epochs.push(epoch);
        test_cases.push(test_case);
    }
}

#[maybe_async::maybe_async]
async fn commit_by_value<F, C: MlsConfig>(
    group: &mut Group<C>,
    proposal_adder: F,
    partial_test_case: TestCase,
) -> TestCase
where
    F: FnOnce(CommitBuilder<C>) -> CommitBuilder<C>,
{
    let builder = proposal_adder(group.commit_builder());
    let commit = builder.build().await.unwrap().commit_message;
    group.apply_pending_commit().await.unwrap();
    let auth = group.epoch_authenticator().unwrap().to_vec();
    let epoch = TestEpoch::new(vec![], &commit, auth);
    let mut test_case = partial_test_case;
    test_case.epochs.push(epoch);
    test_case
}

#[maybe_async::maybe_async]
async fn create_key_package(cs: CipherSuite) -> MLSMessage {
    let client = generate_basic_client(cs, 0xbeef, &Default::default(), &TestCryptoProvider::new());

    client
        .client
        .generate_key_package_message(ProtocolVersion::MLS_10, cs, client.identity)
        .await
        .unwrap()
}

#[maybe_async::maybe_async]
pub async fn generate_passive_client_welcome_tests() {
    let mut test_cases: Vec<TestCase> = vec![];

    for cs in CipherSuite::all() {
        let crypto_provider = TestCryptoProvider::new();
        let Some(cs) = crypto_provider.cipher_suite_provider(cs) else { continue };

        for with_tree_in_extension in [true, false] {
            for (with_psk, with_path) in [false, true].into_iter().cartesian_product([true, false])
            {
                let mut groups = get_test_groups(
                    ProtocolVersion::MLS_10,
                    cs.cipher_suite(),
                    16,
                    &Preferences::default()
                        .with_ratchet_tree_extension(with_tree_in_extension)
                        .force_commit_path_update(with_path),
                    &crypto_provider,
                )
                .await;

                // Remove a member s.t. the passive member joins in their place
                let proposal = groups[0].propose_remove(7, vec![]).await.unwrap();
                all_process_message(&mut groups, &proposal, 0, false).await;

                let mut test_case = invite_passive_client(&mut groups, with_psk, &cs).await;

                if !with_tree_in_extension {
                    let tree = groups[0].export_tree().unwrap();
                    test_case.ratchet_tree = Some(TestRatchetTree(tree));
                }

                test_cases.push(test_case);
            }
        }
    }
}

#[maybe_async::maybe_async]
pub async fn generate_passive_client_random_tests() {
    let mut test_cases: Vec<TestCase> = vec![];
    let version = ProtocolVersion::MLS_10;

    for cs in CipherSuite::all() {
        let crypto = TestCryptoProvider::new();
        let Some(csp) = crypto.cipher_suite_provider(cs) else { continue };

        let creator = generate_basic_client(cs, 0, &Default::default(), &crypto);

        let creator_group = creator
            .client
            .create_group(version, cs, creator.identity, Default::default())
            .await
            .unwrap();

        let mut groups = vec![creator_group];

        let new_clients = (0..10)
            .map(|i| generate_basic_client(cs, i + 1, &Default::default(), &crypto))
            .collect();

        add_random_members(0, &mut groups, new_clients, None).await;

        let mut test_case = invite_passive_client(&mut groups, false, &csp).await;

        let passive_client_index = 11;

        let seed: <rand::rngs::StdRng as SeedableRng>::Seed = rand::random();
        let mut rng = rand::rngs::StdRng::from_seed(seed);
        #[cfg(feature = "std")]
        println!("generating random commits for seed {}", hex::encode(seed));

        let mut next_free_idx = 0;
        for _ in 0..100 {
            // We keep the passive client and another member to send
            let num_removed = rng.gen_range(0..groups.len() - 2);
            let num_added = rng.gen_range(1..30);

            let mut members = (0..groups.len())
                .filter(|i| groups[*i].current_member_index() != passive_client_index)
                .choose_multiple(&mut rng, num_removed + 1);

            let sender = members.pop().unwrap();

            remove_members(members, sender, &mut groups, Some(&mut test_case)).await;

            let sender = (0..groups.len())
                .filter(|i| groups[*i].current_member_index() != passive_client_index)
                .choose(&mut rng)
                .unwrap();

            let new_clients = (0..num_added)
                .map(|i| generate_basic_client(cs, next_free_idx + i, &Default::default(), &crypto))
                .collect();

            add_random_members(sender, &mut groups, new_clients, Some(&mut test_case)).await;

            next_free_idx += num_added;
        }

        test_cases.push(test_case);
    }
}

#[maybe_async::maybe_async]
pub async fn add_random_members<C: MlsConfig>(
    committer: usize,
    groups: &mut Vec<Group<C>>,
    clients: Vec<TestClient<C>>,
    test_case: Option<&mut TestCase>,
) {
    let cipher_suite = groups[committer].cipher_suite();
    let committer_index = groups[committer].current_member_index() as usize;

    let mut key_packages = Vec::new();

    for client in &clients {
        let key_package = client
            .client
            .generate_key_package_message(
                ProtocolVersion::MLS_10,
                cipher_suite,
                client.identity.clone(),
            )
            .await
            .unwrap();

        key_packages.push(key_package);
    }

    let mut add_proposals = Vec::new();

    let committer_group = &mut groups[committer];

    for key_package in key_packages {
        add_proposals.push(
            committer_group
                .propose_add(key_package, vec![])
                .await
                .unwrap(),
        );
    }

    for p in &add_proposals {
        all_process_message(groups, p, committer_index, false).await;
    }

    let commit_output = groups[committer].commit(vec![]).await.unwrap();

    all_process_message(groups, &commit_output.commit_message, committer_index, true).await;

    let auth = groups[committer].epoch_authenticator().unwrap().to_vec();
    let epoch = TestEpoch::new(add_proposals, &commit_output.commit_message, auth);

    if let Some(tc) = test_case {
        tc.epochs.push(epoch)
    };

    let tree_data = groups[committer].export_tree().unwrap();

    for client in &clients {
        let tree_data = tree_data.clone();
        let commit = commit_output.welcome_message.clone().unwrap();

        let group = client
            .client
            .join_group(Some(&tree_data.clone()), commit)
            .await
            .unwrap()
            .0;

        groups.push(group);
    }
}

#[maybe_async::maybe_async]
pub async fn remove_members<C: MlsConfig>(
    removed_members: Vec<usize>,
    committer: usize,
    groups: &mut Vec<Group<C>>,
    test_case: Option<&mut TestCase>,
) {
    let remove_indexes = removed_members
        .iter()
        .map(|removed| groups[*removed].current_member_index())
        .collect::<Vec<u32>>();

    let mut commit_builder = groups[committer].commit_builder();

    for index in remove_indexes {
        commit_builder = commit_builder.remove_member(index).unwrap();
    }

    let commit = commit_builder.build().await.unwrap().commit_message;
    let committer_index = groups[committer].current_member_index() as usize;
    all_process_message(groups, &commit, committer_index, true).await;

    let auth = groups[committer].epoch_authenticator().unwrap().to_vec();
    let epoch = TestEpoch::new(vec![], &commit, auth);

    if let Some(tc) = test_case {
        tc.epochs.push(epoch)
    };

    let mut index = 0;

    groups.retain(|_| {
        index += 1;
        !(removed_members.contains(&(index - 1)))
    });
}
