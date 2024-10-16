use std::str::FromStr;
use std::env;
use std::sync::Arc;

use bip39::Mnemonic;
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::ffi::types::AlignedType;
use bitcoin::NetworkKind;
use blake2::Blake2bVar;
use blake2::digest::{Update, VariableOutput};
use tokio::sync::Mutex;
use tonic::{transport::Server, Request, Response, Status};

use key_share::coordinator_server::{Coordinator, CoordinatorServer};
use key_share::{AddMnemonicReply, AddMnemonicRequest, KeyListReply};

use serde_json::{json, Value};

const SHAMIR_SHARES: usize = 3;
const SHAMIR_THRESHOLD: usize = 2;

pub mod key_share {
    tonic::include_proto!("keyshare"); // The string specified here must match the proto package name
}

#[derive(Debug, Default, PartialEq)]
pub struct KeyShare {
    key_hex: String,
    index: u32,
}

#[derive(Debug, Default)]
pub struct MyCoordinator {
    key_shares: Arc<Mutex<Vec<KeyShare>>>,
}

fn xor_buffers(buf: &[u8; 32], mnemonic: &Vec<u8>) -> Result<Vec<u8>, String> {
    // Check if the size of the mnemonic is exactly 32 bytes
    if mnemonic.len() != 32 {
        return Err("mnemonic must be exactly 32 bytes".to_string());
    }

    // Perform XOR operation between the elements of the array and the vector
    let result = buf.iter()
                    .zip(mnemonic.iter())
                    .map(|(&x, &y)| x ^ y)
                    .collect::<Vec<u8>>();

    Ok(result)
}


impl MyCoordinator {

    async fn add_share(&self, key_hex: String, index: u32) -> Result<String, Status> {
        let mut shares = self.key_shares.lock().await;

        if shares.len() >= SHAMIR_SHARES {
            return Ok("Enough key shares have already been added.".to_string());
        }

        let new_key_share = KeyShare { key_hex, index };

        // Check for duplicates
        if shares.iter().any(|ks| ks.key_hex == new_key_share.key_hex || ks.index == new_key_share.index) {
            return Ok("Key already exists.".to_string());
        } else {
            shares.push(new_key_share); // Insert the new KeyShare if no duplicates are found
        }

        let mut message = "Key added successfully".to_string();

        let mut secret_shares: Vec<Vec<u8>> = Vec::new();
        let mut indexes: Vec<usize> = Vec::new();

        if shares.len() >= SHAMIR_THRESHOLD {

            for share in shares.iter() {
                let ks = hex::decode(share.key_hex.to_string()).unwrap();
                secret_shares.push(ks);
                indexes.push(share.index as usize);
            }

            let secret = bc_shamir::recover_secret(&indexes, &secret_shares).unwrap();

            let network_kind = get_network_kind();

            // we need secp256k1 context for key derivation
            let mut buf: Vec<AlignedType> = Vec::new();
            buf.resize(Secp256k1::preallocate_size(), AlignedType::zeroed());
            let secp = Secp256k1::preallocated_new(buf.as_mut_slice()).unwrap();
            
            let derivation_path = env::var("DERIVATION_PATH").unwrap_or_else(|_| "m/84'/0'/0'/0/0".into());

            let root = Xpriv::new_master(network_kind, &secret).unwrap();
            let path = DerivationPath::from_str(&derivation_path).unwrap();
            let child = root.derive_priv(&secp, &path).unwrap();
            let secret_key = child.private_key;

            let seed_content =  hex::encode(secret_key.secret_bytes());

            message += " and secret recovered.";

            let result = send_seed(&seed_content).await;

            match result {
                Ok(_) => {
                    message += " Secret sent to server.";
                },
                Err(e) => {
                    message += &format!(" Error sending secret to server: {}", e);
                }
            }
        }

        Ok(message)
    }

}

#[tonic::async_trait]
impl Coordinator for MyCoordinator {

    async fn add_mnemonic(
        &self,
        request: Request<AddMnemonicRequest>,
    ) -> Result<Response<AddMnemonicReply>, Status> {

        let request_inner = request.into_inner();
        let mnemonic_str = request_inner.mnemonic;
        let index = request_inner.index;
        let password = request_inner.password;

        let password = password.as_bytes();

        let mut hasher = Blake2bVar::new(32).unwrap();
        hasher.update(password);
        let mut buf = [0u8; 32];
        hasher.finalize_variable(&mut buf).unwrap();

        let mnemonic = Mnemonic::parse(&mnemonic_str).unwrap();

        let xor_result = xor_buffers(&buf, &mnemonic.to_entropy()).unwrap();

        let key_hex = hex::encode(xor_result);

        let message = self.add_share(key_hex, index).await?;

        Ok(Response::new(AddMnemonicReply { message }))
    }

    async fn list_keys(&self, _request: Request<()>) -> Result<Response<KeyListReply>, Status> {

        let mut message = KeyListReply::default();

        let shares = self.key_shares.lock().await;

        for key_share in &shares[..] {
            message.items.push(key_share.key_hex.to_string());
        }

        Ok(Response::new(message))

    }
}

pub fn get_network_kind() -> bitcoin::network::NetworkKind {
    let network = env::var("NETWORK").unwrap_or_else(|_| "bitcoin".into());
    match network.as_str() {
        "signet" => NetworkKind::Test,
        "testnet" => NetworkKind::Test,
        "regtest" => NetworkKind::Test,
        "bitcoin" => NetworkKind::Main,
        _ => NetworkKind::Main, // Default case to handle unexpected values
    }
}

async fn send_seed(seed: &str) -> Result<(), Box<dyn std::error::Error>> {

    let client = reqwest::Client::new();

    let response  = client
        .post("http://localhost:5000/uploadsecret")
        .json(&json!({ "secret": seed }))
        .send()
        .await?;

    if response.status().is_success() {
        let body: Value = response.json().await?;
        println!("Server response: {:?}", body);
        return Ok(());
    } else {
        return Err(format!("Request failed with status: {}", response.status()).into());
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    // let addr = "[::1]:50051".parse()?;
    let addr = "127.0.0.1:50051".parse()?;
    let coordinator = MyCoordinator::default();

    println!("Server started at {}", addr);

    Server::builder()
        .add_service(CoordinatorServer::new(coordinator))
        .serve(addr)
        .await?;

    Ok(())
}
