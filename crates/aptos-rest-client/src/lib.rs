// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, Result};
pub use aptos_api_types::{
    self, IndexResponse, MoveModuleBytecode, PendingTransaction, Transaction,
};
use aptos_api_types::{
    mime_types::BCS_SIGNED_TRANSACTION as BCS_CONTENT_TYPE, Block, BlockInfo, Event,
};
use aptos_crypto::HashValue;
use aptos_types::{
    account_address::AccountAddress,
    account_config::{NewBlockEvent, CORE_CODE_ADDRESS},
    transaction::SignedTransaction,
};
use reqwest::{header::CONTENT_TYPE, Client as ReqwestClient, StatusCode};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
pub use state::State;
use std::time::Duration;
use url::Url;

pub mod error;
pub mod faucet;
pub use faucet::FaucetClient;
pub mod response;
pub use response::Response;
pub mod state;
pub mod types;
use crate::aptos::{AptosVersion, Balance};
pub use types::{Account, Resource, RestError};
pub mod aptos;
use types::deserialize_from_string;

pub const USER_AGENT: &str = concat!("aptos-client-sdk-rust / ", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
pub struct Client {
    inner: ReqwestClient,
    base_url: Url,
}

impl Client {
    pub fn new(base_url: Url) -> Self {
        let inner = ReqwestClient::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .cookie_store(true)
            .build()
            .unwrap();

        Self { inner, base_url }
    }

    pub async fn get_aptos_version(&self) -> Result<Response<AptosVersion>> {
        self.get_resource::<AptosVersion>(CORE_CODE_ADDRESS, "0x1::version::Version")
            .await
    }

    pub async fn get_block(&self, height: u64, with_transactions: bool) -> Result<Response<Block>> {
        self.get(self.base_url.join(&format!(
            "v1/blocks/by_height/{}?with_transactions={}",
            height, with_transactions
        ))?)
        .await
    }

    pub async fn get_block_info(&self, version: u64) -> Result<Response<BlockInfo>> {
        self.get(self.base_url.join(&format!("blocks/{}", version))?)
            .await
    }

    pub async fn get_account_balance(&self, address: AccountAddress) -> Result<Response<Balance>> {
        let resp = self
            .get_account_resource(address, "0x1::coin::CoinStore<0x1::aptos_coin::AptosCoin>")
            .await?;
        resp.and_then(|resource| {
            if let Some(res) = resource {
                Ok(serde_json::from_value::<Balance>(res.data)?)
            } else {
                Err(anyhow!("No data returned"))
            }
        })
    }

    pub async fn get_index(&self) -> Result<Response<IndexResponse>> {
        self.get(self.base_url.clone()).await
    }

    pub async fn get_ledger_information(&self) -> Result<Response<State>> {
        #[derive(Deserialize)]
        struct Response {
            chain_id: u8,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            epoch: u64,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            ledger_version: u64,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            oldest_ledger_version: u64,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            block_height: u64,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            oldest_block_height: u64,
            #[serde(deserialize_with = "types::deserialize_from_string")]
            ledger_timestamp: u64,
        }

        let response = self
            .get::<Response>(self.base_url.clone())
            .await?
            .map(|r| State {
                chain_id: r.chain_id,
                epoch: r.epoch,
                version: r.ledger_version,
                timestamp_usecs: r.ledger_timestamp,
                oldest_ledger_version: r.oldest_ledger_version,
                oldest_block_height: r.oldest_block_height,
                block_height: r.block_height,
            });

        Ok(response)
    }

    pub async fn submit(&self, txn: &SignedTransaction) -> Result<Response<PendingTransaction>> {
        let txn_payload = bcs::to_bytes(txn)?;
        let url = self.base_url.join("transactions")?;

        let response = self
            .inner
            .post(url)
            .header(CONTENT_TYPE, BCS_CONTENT_TYPE)
            .body(txn_payload)
            .send()
            .await?;

        self.json(response).await
    }

    pub async fn submit_and_wait(&self, txn: &SignedTransaction) -> Result<Response<Transaction>> {
        self.submit(txn).await?;
        self.wait_for_signed_transaction(txn).await
    }

    pub async fn wait_for_transaction(
        &self,
        pending_transaction: &PendingTransaction,
    ) -> Result<Response<Transaction>> {
        self.wait_for_transaction_by_hash(
            pending_transaction.hash.into(),
            *pending_transaction
                .request
                .expiration_timestamp_secs
                .inner(),
        )
        .await
    }

    pub async fn wait_for_signed_transaction(
        &self,
        transaction: &SignedTransaction,
    ) -> Result<Response<Transaction>> {
        let expiration_timestamp = transaction.expiration_timestamp_secs();
        self.wait_for_transaction_by_hash(
            transaction.clone().committed_hash(),
            expiration_timestamp,
        )
        .await
    }

    pub async fn wait_for_transaction_by_hash(
        &self,
        hash: HashValue,
        expiration_timestamp_secs: u64,
    ) -> Result<Response<Transaction>> {
        const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
        const DEFAULT_DELAY: Duration = Duration::from_millis(500);

        let start = std::time::Instant::now();
        while start.elapsed() < DEFAULT_TIMEOUT {
            let resp = self
                .get_transaction_by_version_or_hash(hash.to_hex_literal())
                .await?;
            if resp.status() != StatusCode::NOT_FOUND {
                let txn_resp: Response<Transaction> = self.json(resp).await?;
                let (transaction, state) = txn_resp.into_parts();

                if !transaction.is_pending() {
                    if !transaction.success() {
                        return Err(anyhow!(
                            "transaction execution failed: {}",
                            transaction.vm_status()
                        ));
                    }
                    return Ok(Response::new(transaction, state));
                }
                if expiration_timestamp_secs <= state.timestamp_usecs / 1_000_000 {
                    return Err(anyhow!("transaction expired"));
                }
            }

            tokio::time::sleep(DEFAULT_DELAY).await;
        }

        Err(anyhow!("timeout"))
    }

    pub async fn get_transactions(
        &self,
        start: Option<u64>,
        limit: Option<u16>,
    ) -> Result<Response<Vec<Transaction>>> {
        let url = self.base_url.join("transactions")?;

        let mut request = self.inner.get(url);
        if let Some(start) = start {
            request = request.query(&[("start", start)])
        }

        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)])
        }

        let response = request.send().await?;

        self.json(response).await
    }

    pub async fn get_transaction(&self, hash: HashValue) -> Result<Response<Transaction>> {
        self.json(
            self.get_transaction_by_version_or_hash(hash.to_hex_literal())
                .await?,
        )
        .await
    }

    pub async fn get_transaction_by_version(&self, version: u64) -> Result<Response<Transaction>> {
        self.json(
            self.get_transaction_by_version_or_hash(version.to_string())
                .await?,
        )
        .await
    }

    async fn get_transaction_by_version_or_hash(
        &self,
        version_or_hash: String,
    ) -> Result<reqwest::Response> {
        let url = self
            .base_url
            .join(&format!("transactions/{}", version_or_hash))?;

        Ok(self.inner.get(url).send().await?)
    }

    pub async fn get_account_transactions(
        &self,
        address: AccountAddress,
        start: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Response<Vec<Transaction>>> {
        let url = self
            .base_url
            .join(&format!("accounts/{}/transactions", address))?;

        let mut request = self.inner.get(url);
        if let Some(start) = start {
            request = request.query(&[("start", start)])
        }

        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)])
        }

        let response = request.send().await?;

        self.json(response).await
    }

    pub async fn get_account_resources(
        &self,
        address: AccountAddress,
    ) -> Result<Response<Vec<Resource>>> {
        let url = self
            .base_url
            .join(&format!("accounts/{}/resources", address))?;

        let response = self.inner.get(url).send().await?;

        self.json(response).await
    }

    pub async fn get_account_resources_at_version(
        &self,
        address: AccountAddress,
        version: u64,
    ) -> Result<Response<Vec<Resource>>> {
        let url = self.base_url.join(&format!(
            "accounts/{}/resources?version={}",
            address, version
        ))?;

        let response = self.inner.get(url).send().await?;

        self.json(response).await
    }

    pub async fn get_resource<T: DeserializeOwned>(
        &self,
        address: AccountAddress,
        resource_type: &str,
    ) -> Result<Response<T>> {
        let resp = self.get_account_resource(address, resource_type).await?;
        resp.and_then(|conf| {
            if let Some(res) = conf {
                serde_json::from_value(res.data)
                    .map_err(|e| anyhow!("deserialize {} failed: {}", resource_type, e))
            } else {
                Err(anyhow!(
                    "could not find resource {} in account {}",
                    resource_type,
                    address
                ))
            }
        })
    }

    pub async fn get_account_resource(
        &self,
        address: AccountAddress,
        resource_type: &str,
    ) -> Result<Response<Option<Resource>>> {
        let url = self
            .base_url
            .join(&format!("accounts/{}/resource/{}", address, resource_type))?;

        let response = self.inner.get(url).send().await?;
        self.json(response).await
    }

    pub async fn get_account_resource_at_version(
        &self,
        address: AccountAddress,
        resource_type: &str,
        version: u64,
    ) -> Result<Response<Option<Resource>>> {
        let url = self.base_url.join(&format!(
            "accounts/{}/resource/{}?version={}",
            address, resource_type, version
        ))?;

        let response = self.inner.get(url).send().await?;
        self.json(response).await
    }

    pub async fn get_account_modules(
        &self,
        address: AccountAddress,
    ) -> Result<Response<Vec<MoveModuleBytecode>>> {
        let url = self
            .base_url
            .join(&format!("accounts/{}/modules", address))?;

        let response = self.inner.get(url).send().await?;
        self.json(response).await
    }

    pub async fn get_account_events(
        &self,
        address: AccountAddress,
        struct_tag: &str,
        field_name: &str,
        start: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Response<Vec<Event>>> {
        let url = self.base_url.join(&format!(
            "accounts/{}/events/{}/{}",
            address.to_hex_literal(),
            struct_tag,
            field_name
        ))?;
        let mut request = self.inner.get(url);
        if let Some(start) = start {
            request = request.query(&[("start", start)])
        }

        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)])
        }

        let response = request.send().await?;
        self.json(response).await
    }

    pub async fn get_new_block_events(
        &self,
        start: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Response<Vec<NewBlockEvent>>> {
        #[derive(Clone, Debug, Serialize, Deserialize)]
        pub struct NewBlockEventResponse {
            #[serde(deserialize_with = "deserialize_from_string")]
            epoch: u64,
            #[serde(deserialize_with = "deserialize_from_string")]
            round: u64,
            #[serde(deserialize_with = "deserialize_from_string")]
            height: u64,
            previous_block_votes: Vec<bool>,
            proposer: String,
            failed_proposer_indices: Vec<String>,
            #[serde(deserialize_with = "deserialize_from_string")]
            time_microseconds: u64,
        }

        let response = self
            .get_account_events(
                CORE_CODE_ADDRESS,
                "0x1::block::BlockResource",
                "new_block_events",
                start,
                limit,
            )
            .await?;

        response.and_then(|events| {
            let new_events: Result<Vec<_>> = events
                .into_iter()
                .map(|event| {
                    serde_json::from_value::<NewBlockEventResponse>(event.data)
                        .map_err(|e| anyhow!(e))
                        .and_then(|e| {
                            Ok(NewBlockEvent::new(
                                e.epoch,
                                e.round,
                                e.height,
                                e.previous_block_votes,
                                AccountAddress::from_hex_literal(&e.proposer)
                                    .map_err(|e| anyhow!(e))?,
                                e.failed_proposer_indices
                                    .iter()
                                    .map(|v| v.parse())
                                    .collect::<Result<Vec<_>, _>>()?,
                                e.time_microseconds,
                            ))
                        })
                })
                .collect();
            new_events
        })
    }

    pub async fn get_table_item<K: Serialize>(
        &self,
        table_handle: u128,
        key_type: &str,
        value_type: &str,
        key: K,
    ) -> Result<Response<Value>> {
        let url = self
            .base_url
            .join(&format!("tables/{}/item", table_handle))?;
        let data = json!({
            "key_type": key_type,
            "value_type": value_type,
            "key": json!(key),
        });

        let response = self.inner.post(url).json(&data).send().await?;
        self.json(response).await
    }

    pub async fn get_account(&self, address: AccountAddress) -> Result<Response<Account>> {
        let url = self.base_url.join(&format!("accounts/{}", address))?;
        let response = self.inner.get(url).send().await?;
        self.json(response).await
    }

    pub async fn set_failpoint(&self, name: String, actions: String) -> Result<String> {
        let mut base = self.base_url.join("set_failpoint")?;
        let url = base
            .query_pairs_mut()
            .append_pair("name", &name)
            .append_pair("actions", &actions)
            .finish();
        let response = self.inner.get(url.clone()).send().await?;

        if !response.status().is_success() {
            let error_response = response.json::<RestError>().await?;
            return Err(anyhow::anyhow!("Request failed: {:?}", error_response));
        }

        response
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("To text failed: {:?}", e))
    }

    async fn check_response(
        &self,
        response: reqwest::Response,
    ) -> Result<(reqwest::Response, State)> {
        if !response.status().is_success() {
            let error_response = response.json::<RestError>().await?;
            return Err(anyhow::anyhow!("Request failed: {:?}", error_response));
        }
        let state = State::from_headers(response.headers())?;

        Ok((response, state))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<Response<T>> {
        let (response, state) = self.check_response(response).await?;
        let json = response.json().await?;
        Ok(Response::new(json, state))
    }

    pub async fn health_check(&self, seconds: u64) -> Result<()> {
        let url = self.base_url.join("-/healthy")?;
        let response = self
            .inner
            .get(url)
            .query(&[("duration_secs", seconds)])
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(anyhow::anyhow!("health check failed"));
        }

        Ok(())
    }

    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<Response<T>> {
        self.json(self.inner.get(url).send().await?).await
    }
}

impl From<(ReqwestClient, Url)> for Client {
    fn from((inner, base_url): (ReqwestClient, Url)) -> Self {
        Client { inner, base_url }
    }
}