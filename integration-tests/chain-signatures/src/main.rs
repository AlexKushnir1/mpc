use std::fs::File;
use std::io::Write;
use std::str::FromStr;
use std::vec;

use clap::Parser;
use integration_tests_chain_signatures::containers::DockerClient;
use integration_tests_chain_signatures::{dry_run, run, utils, MultichainConfig};
use mpc_contract::primitives::SignRequest;
use near_account_id::AccountId;
use serde_json::json;
use tokio::signal;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
enum Cli {
    /// Spin up dependent services and mpc nodes
    SetupEnv {
        #[arg(short, long, default_value_t = 3)]
        nodes: usize,
        #[arg(short, long, default_value_t = 2)]
        threshold: usize,
    },
    /// Spin up dependent services but not mpc nodes
    DepServices,
    /// Example of commands to interact with the contract
    ContractCommands,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_thread_ids(true)
        .with_env_filter(EnvFilter::from_default_env());
    subscriber.init();
    let docker_client = DockerClient::default();

    match Cli::parse() {
        Cli::SetupEnv { nodes, threshold } => {
            println!(
                "Setting up an environment with {} nodes, {} threshold ...",
                nodes, threshold
            );
            let config = MultichainConfig {
                nodes,
                threshold,
                ..Default::default()
            };
            println!("Full config: {:?}", config);
            let nodes = run(config.clone(), &docker_client).await?;
            let ctx = nodes.ctx();
            let urls: Vec<_> = (0..config.nodes).map(|i| nodes.url(i)).collect();
            let near_accounts = nodes.near_accounts();
            let sk_local_path = nodes.ctx().storage_options.sk_share_local_path.clone();

            println!("\nEnvironment is ready:");
            println!("  docker-network: {}", ctx.docker_network);
            println!("  release:        {}", ctx.release);

            println!("\nExternal services:");
            println!("  datastore:     {}", ctx.datastore.local_address);
            println!("  lake_indexer:  {}", ctx.lake_indexer.rpc_host_address);

            println!("\nNodes:");
            for i in 0..urls.len() {
                println!("  Node {}", i);
                println!("    Url: {}", urls[i]);
                let account_id = near_accounts[i].id();
                println!("    Account: {}", account_id);
                let sk = near_accounts[i].secret_key();
                println!("    Secret Key: {}", sk);
                let pk = sk.public_key();
                println!("    Public Key: {}", pk);
            }

            signal::ctrl_c().await.expect("Failed to listen for event");
            println!("Received Ctrl-C");
            utils::clear_local_sk_shares(sk_local_path).await?;
            println!("Clean up finished");
        }
        Cli::DepServices => {
            println!("Setting up dependency services");
            let config = MultichainConfig::default();
            let _ctx = dry_run(config.clone(), &docker_client).await?;

            println!("Press Ctrl-C to stop dependency services");
            signal::ctrl_c().await.expect("Failed to listen for event");
            println!("Received Ctrl-C");
            println!("Stopped dependency services");
        }
        Cli::ContractCommands => {
            println!("Building an example contract command");
            let path_to_args_example = "../../chain-signatures/contract/src/json_args.sh";
            let mut file = File::create(path_to_args_example)?;
            let mut commands: Vec<String> = vec![];
            let contract_account_id = AccountId::from_str("v1.signer-dev.testnet").unwrap();
            let caller_account_id = AccountId::from_str("alexkushnir.testnet").unwrap();

            let sign_request = SignRequest {
                payload: [
                    12, 1, 2, 0, 4, 5, 6, 8, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
                    22, 23, 24, 25, 26, 27, 28, 29, 30, 44,
                ],
                path: "test".into(),
                key_version: 0,
            };

            let request_json = format!(
                "'{}'",
                serde_json::to_string(&json!({"request": sign_request})).unwrap()
            );

            let sign_command = format!(
                "near call {} sign {} --accountId {} --gas 300000000000000 --deposit 1",
                contract_account_id, request_json, caller_account_id
            );

            commands.push(sign_command.clone());

            let public_key_command = format!(
                "near call {} public_key --accountId {} --gas 300000000000000 --deposit 1",
                contract_account_id, caller_account_id
            );

            commands.push(public_key_command.clone());

            let derived_pub_key_json = format!(
                "'{}'",
                serde_json::to_string(&json!({"path": "test","predecessor": caller_account_id})).unwrap()
            );

            let derived_public_key_command = format!(
                "near call {} derived_public_key {} --accountId {} --gas 300000000000000 --deposit 1",
                contract_account_id, derived_pub_key_json, caller_account_id
            );

            commands.push(derived_public_key_command.clone());

            for arg in commands {
                file.write_all(arg.as_bytes())?;
                file.write_all("\n\n".as_bytes())?;
            }
        }
    }

    Ok(())
}
