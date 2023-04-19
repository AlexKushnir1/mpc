use crate::docker::{LeaderNode, SignNode};
use bollard::Docker;
use docker::{redis::Redis, relayer::Relayer};
use futures::future::BoxFuture;
use mpc_recovery::msg::{
    AddKeyRequest, AddKeyResponse, LeaderRequest, LeaderResponse, NewAccountRequest,
    NewAccountResponse,
};
use rand::{distributions::Alphanumeric, Rng};
use std::time::Duration;
use threshold_crypto::PublicKeySet;
use workspaces::{network::Sandbox, AccountId, Worker};

mod docker;

const NETWORK: &str = "mpc_recovery_integration_test_network";

struct TestContext<'a> {
    leader_node: &'a LeaderNode,
    pk_set: &'a PublicKeySet,
    worker: &'a Worker<Sandbox>,
}

async fn create_account(
    worker: &Worker<Sandbox>,
) -> anyhow::Result<(AccountId, near_crypto::SecretKey)> {
    let (account_id, account_sk) = worker.dev_generate().await;
    worker
        .create_tla(account_id.clone(), account_sk.clone())
        .await?
        .into_result()?;

    let account_sk: near_crypto::SecretKey =
        serde_json::from_str(&serde_json::to_string(&account_sk)?)?;

    Ok((account_id, account_sk))
}

async fn with_nodes<F>(shares: usize, threshold: usize, nodes: usize, f: F) -> anyhow::Result<()>
where
    F: for<'a> FnOnce(TestContext<'a>) -> BoxFuture<'a, anyhow::Result<()>>,
{
    let docker = Docker::connect_with_local_defaults()?;

    let (pk_set, sk_shares) = mpc_recovery::generate(shares, threshold)?;
    let worker = workspaces::sandbox().await?;
    let near_root_account = worker.root_account()?;
    near_root_account
        .deploy(include_bytes!("../linkdrop.wasm"))
        .await?
        .into_result()?;
    near_root_account
        .call(near_root_account.id(), "new")
        .max_gas()
        .transact()
        .await?
        .into_result()?;
    let (relayer_account_id, relayer_account_sk) = create_account(&worker).await?;
    let (creator_account_id, creator_account_sk) = create_account(&worker).await?;

    let near_rpc = format!("http://172.17.0.1:{}", worker.rpc_port());
    let redis = Redis::start(&docker, NETWORK).await?;
    let relayer = Relayer::start(
        &docker,
        NETWORK,
        &near_rpc,
        &redis.hostname,
        &relayer_account_id,
        &relayer_account_sk,
        &creator_account_id,
    )
    .await?;

    let mut sign_nodes = Vec::new();
    for i in 2..=nodes {
        let addr = SignNode::start(&docker, NETWORK, i as u64, &pk_set, &sk_shares[i - 1]).await?;
        sign_nodes.push(addr);
    }
    let leader_node = LeaderNode::start(
        &docker,
        NETWORK,
        1,
        &pk_set,
        &sk_shares[0],
        sign_nodes.iter().map(|n| n.address.clone()).collect(),
        &near_rpc,
        &relayer.address,
        near_root_account.id(),
        &creator_account_id,
        &creator_account_sk,
    )
    .await?;

    // Wait until all nodes initialize
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let result = f(TestContext {
        leader_node: &leader_node,
        pk_set: &pk_set,
        worker: &worker,
    })
    .await;

    drop(leader_node);
    drop(sign_nodes);
    drop(relayer);
    drop(redis);

    // Wait until all docker containers are destroyed.
    // See `Drop` impl for `LeaderNode` and `SignNode` for more info.
    tokio::time::sleep(Duration::from_millis(2000)).await;

    result
}

#[tokio::test]
async fn test_trio() -> anyhow::Result<()> {
    with_nodes(4, 3, 3, |ctx| {
        Box::pin(async move {
            let payload: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();
            let (status_code, response) = ctx
                .leader_node
                .submit(LeaderRequest {
                    payload: payload.clone(),
                })
                .await?;

            assert_eq!(status_code, 200);
            if let LeaderResponse::Ok { signature } = response {
                assert!(ctx.pk_set.public_key().verify(&signature, payload));
            } else {
                panic!("response was not successful");
            }

            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn test_basic_action() -> anyhow::Result<()> {
    with_nodes(4, 3, 3, |ctx| {
        Box::pin(async move {
            // Create new account
            // TODO: write a test with real token
            // "validToken" should triger test token verifyer and return success
            let id_token = "validToken".to_string();
            let account_id_rand: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(10)
                .map(char::from)
                .collect();
            let account_id: AccountId = format!(
                "mpc-recovery-{}.{}",
                account_id_rand.to_lowercase(),
                ctx.worker.root_account()?.id()
            )
            .parse()
            .unwrap();

            let user_public_key =
                near_crypto::SecretKey::from_random(near_crypto::KeyType::ED25519)
                    .public_key()
                    .to_string();

            let (status_code, new_acc_response) = ctx
                .leader_node
                .new_account(NewAccountRequest {
                    near_account_id: account_id.to_string(),
                    oidc_token: id_token.clone(),
                    public_key: user_public_key.clone(),
                })
                .await
                .unwrap();
            assert_eq!(status_code, 200);
            assert!(matches!(new_acc_response, NewAccountResponse::Ok));

            tokio::time::sleep(Duration::from_millis(2000)).await;

            // Check that account exists and it has the requested public key
            let access_keys = ctx.worker.view_access_keys(&account_id).await?;
            assert!(access_keys
                .iter()
                .any(|ak| ak.public_key.to_string() == user_public_key));

            let new_user_public_key =
                near_crypto::SecretKey::from_random(near_crypto::KeyType::ED25519)
                    .public_key()
                    .to_string();

            let (status_code2, add_key_response) = ctx
                .leader_node
                .add_key(AddKeyRequest {
                    near_account_id: account_id.to_string(),
                    oidc_token: id_token.clone(),
                    public_key: new_user_public_key.clone(),
                })
                .await?;

            assert_eq!(status_code2, 200);
            assert!(matches!(add_key_response, AddKeyResponse::Ok));

            tokio::time::sleep(Duration::from_millis(2000)).await;

            // Check that account has the requested public key
            let access_keys = ctx.worker.view_access_keys(&account_id).await?;
            assert!(access_keys
                .iter()
                .any(|ak| ak.public_key.to_string() == new_user_public_key));

            Ok(())
        })
    })
    .await
}
