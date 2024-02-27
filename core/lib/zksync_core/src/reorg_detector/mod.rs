use std::{fmt, time::Duration};

use anyhow::Context as _;
use async_trait::async_trait;
use tokio::sync::watch;
use zksync_dal::ConnectionPool;
use zksync_health_check::{Health, HealthStatus, HealthUpdater, ReactiveHealthCheck};
use zksync_types::{L1BatchNumber, MiniblockNumber, H256};
use zksync_web3_decl::{
    error::{ClientRpcContext, EnrichedClientError, EnrichedClientResult},
    jsonrpsee::{core::ClientError as RpcError, http_client::HttpClient},
    namespaces::{EthNamespaceClient, ZksNamespaceClient},
};

use crate::{
    metrics::{CheckerComponent, EN_METRICS},
    utils::{binary_search_with, wait_for_l1_batch_with_metadata},
};

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("RPC error calling main node: {0}")]
    Rpc(#[from] EnrichedClientError),
    #[error(
        "Unrecoverable error: the earliest L1 batch #{0} in the local DB \
        has mismatched hash with the main node. Make sure you're connected to the right network; \
        if you've recovered from a snapshot, re-check snapshot authenticity. \
        Using an earlier snapshot could help."
    )]
    EarliestHashMismatch(L1BatchNumber),
    #[error(
        "Unrecoverable error: the earliest L1 batch #{0} in the local DB \
        is truncated on the main node. Make sure you're connected to the right network; \
        if you've recovered from a snapshot, re-check snapshot authenticity. \
        Using an earlier snapshot could help."
    )]
    EarliestL1BatchTruncated(L1BatchNumber),
    #[error("reorg detected, restart the node to revert to the last correct L1 batch #{0}.")]
    ReorgDetected(L1BatchNumber),
    #[error("storage error: {0:#}")]
    Storage(anyhow::Error),
    #[error("no remote L1 batch")]
    NoRemoteL1Batch,
}

impl Error {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::NoRemoteL1Batch => true,
            Self::Rpc(err) => match err.as_ref() {
                RpcError::Transport(_) | RpcError::RequestTimeout => true,
                _ => false,
            },
            _ => false,
        }
    }
}

impl From<zksync_dal::SqlxError> for Error {
    fn from(err: zksync_dal::SqlxError) -> Self {
        Self::Storage(err.into())
    }
}

#[async_trait]
trait MainNodeClient: fmt::Debug + Send + Sync {
    async fn sealed_miniblock_number(&self) -> EnrichedClientResult<MiniblockNumber>;

    async fn sealed_l1_batch_number(&self) -> EnrichedClientResult<L1BatchNumber>;

    async fn miniblock_hash(&self, number: MiniblockNumber) -> EnrichedClientResult<Option<H256>>;

    async fn l1_batch_root_hash(&self, number: L1BatchNumber)
        -> EnrichedClientResult<Option<H256>>;
}

#[async_trait]
impl MainNodeClient for HttpClient {
    async fn sealed_miniblock_number(&self) -> EnrichedClientResult<MiniblockNumber> {
        let number = self
            .get_block_number()
            .rpc_context("sealed_miniblock_number")
            .await?;
        let number = u32::try_from(number).map_err(|err| {
            EnrichedClientError::custom(err, "u32::try_from").with_arg("number", &number)
        })?;
        Ok(MiniblockNumber(number))
    }

    async fn sealed_l1_batch_number(&self) -> EnrichedClientResult<L1BatchNumber> {
        let number = self
            .get_l1_batch_number()
            .rpc_context("sealed_l1_batch_number")
            .await?;
        let number = u32::try_from(number).map_err(|err| {
            EnrichedClientError::custom(err, "u32::try_from").with_arg("number", &number)
        })?;
        Ok(L1BatchNumber(number))
    }

    async fn miniblock_hash(&self, number: MiniblockNumber) -> EnrichedClientResult<Option<H256>> {
        Ok(self
            .get_block_by_number(number.0.into(), false)
            .rpc_context("miniblock_hash")
            .with_arg("number", &number)
            .await?
            .map(|block| block.hash))
    }

    async fn l1_batch_root_hash(
        &self,
        number: L1BatchNumber,
    ) -> EnrichedClientResult<Option<H256>> {
        Ok(self
            .get_l1_batch_details(number)
            .rpc_context("l1_batch_root_hash")
            .with_arg("number", &number)
            .await?
            .and_then(|batch| batch.base.root_hash))
    }
}

trait HandleReorgDetectorEvent: fmt::Debug + Send + Sync {
    fn initialize(&mut self);

    fn update_correct_block(
        &mut self,
        last_correct_miniblock: MiniblockNumber,
        last_correct_l1_batch: L1BatchNumber,
    );

    fn report_divergence(&mut self, diverged_l1_batch: L1BatchNumber);

    fn start_shutting_down(&mut self);
}

/// Default implementation of [`HandleReorgDetectorEvent`] that reports values as metrics.
impl HandleReorgDetectorEvent for HealthUpdater {
    fn initialize(&mut self) {
        self.update(Health::from(HealthStatus::Ready));
    }

    fn update_correct_block(
        &mut self,
        last_correct_miniblock: MiniblockNumber,
        last_correct_l1_batch: L1BatchNumber,
    ) {
        let last_correct_miniblock = last_correct_miniblock.0.into();
        let prev_checked_miniblock = EN_METRICS.last_correct_miniblock
            [&CheckerComponent::ReorgDetector]
            .set(last_correct_miniblock);
        if prev_checked_miniblock != last_correct_miniblock {
            tracing::debug!("No reorg at miniblock #{last_correct_miniblock}");
        }

        let last_correct_l1_batch = last_correct_l1_batch.0.into();
        let prev_checked_l1_batch = EN_METRICS.last_correct_batch[&CheckerComponent::ReorgDetector]
            .set(last_correct_l1_batch);
        if prev_checked_l1_batch != last_correct_l1_batch {
            tracing::debug!("No reorg at L1 batch #{last_correct_l1_batch}");
        }

        let health_details = serde_json::json!({
            "last_correct_miniblock": last_correct_miniblock,
            "last_correct_l1_batch": last_correct_l1_batch,
        });
        self.update(Health::from(HealthStatus::Ready).with_details(health_details));
    }

    fn report_divergence(&mut self, diverged_l1_batch: L1BatchNumber) {
        let health_details = serde_json::json!({
            "diverged_l1_batch": diverged_l1_batch,
        });
        self.update(Health::from(HealthStatus::Affected).with_details(health_details));
    }

    fn start_shutting_down(&mut self) {
        self.update(HealthStatus::ShuttingDown.into());
    }
}

/// This is a component that is responsible for detecting the batch re-orgs.
/// Batch re-org is a rare event of manual intervention, when the node operator
/// decides to revert some of the not yet finalized batches for some reason
/// (e.g. inability to generate a proof), and then potentially
/// re-organize transactions in them to fix the problem.
///
/// To detect them, we constantly check the latest sealed batch root hash,
/// and in the event of mismatch, we know that there has been a re-org.
/// We then perform a binary search to find the latest correct block
/// and revert all batches after it, to keep being consistent with the main node.
///
/// This is the only component that is expected to finish its execution
/// in the event of re-org, since we have to restart the node after a rollback is performed,
/// and is special-cased in the `zksync_external_node` crate.
#[derive(Debug)]
pub struct ReorgDetector {
    client: Box<dyn MainNodeClient>,
    event_handler: Box<dyn HandleReorgDetectorEvent>,
    pool: ConnectionPool,
    sleep_interval: Duration,
    health_check: ReactiveHealthCheck,
}

impl ReorgDetector {
    const DEFAULT_SLEEP_INTERVAL: Duration = Duration::from_secs(5);

    pub fn new(client: HttpClient, pool: ConnectionPool) -> Self {
        let (health_check, health_updater) = ReactiveHealthCheck::new("reorg_detector");
        Self {
            client: Box::new(client),
            event_handler: Box::new(health_updater),
            pool,
            sleep_interval: Self::DEFAULT_SLEEP_INTERVAL,
            health_check,
        }
    }

    pub fn health_check(&self) -> &ReactiveHealthCheck {
        &self.health_check
    }

    /// Returns `Ok(())` if no reorg was detected.
    /// Returns `Err::ReorgDetected()` if a reorg was detected.
    pub async fn check_consistency(&mut self) -> Result<(), Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::Storage)?;
        let Some(local_l1_batch) = storage
            .blocks_dal()
            .get_last_l1_batch_number_with_metadata()
            .await?
        else {
            return Ok(());
        };
        let Some(local_miniblock) = storage.blocks_dal().get_sealed_miniblock_number().await?
        else {
            return Ok(());
        };
        drop(storage);

        let remote_l1_batch = self.client.sealed_l1_batch_number().await?;
        let remote_miniblock = self.client.sealed_miniblock_number().await?;

        let checked_l1_batch = local_l1_batch.min(remote_l1_batch);
        let checked_miniblock = local_miniblock.min(remote_miniblock);

        let root_hashes_match = self.root_hashes_match(checked_l1_batch).await?;
        let miniblock_hashes_match = self.miniblock_hashes_match(checked_miniblock).await?;

        // The only event that triggers re-org detection and node rollback is if the
        // hash mismatch at the same block height is detected, be it miniblocks or batches.
        //
        // In other cases either there is only a height mismatch which means that one of
        // the nodes needs to do catching up; however, it is not certain that there is actually
        // a re-org taking place.
        if root_hashes_match && miniblock_hashes_match {
            self.event_handler
                .update_correct_block(checked_miniblock, checked_l1_batch);
            return Ok(());
        }
        let diverged_l1_batch = checked_l1_batch + (root_hashes_match as u32);
        self.event_handler.report_divergence(diverged_l1_batch);

        tracing::info!("Searching for the first diverged L1 batch");
        let mut storage = self.pool.access_storage().await.map_err(Error::Storage)?;
        let earliest_l1_batch = storage
            .blocks_dal()
            .get_earliest_l1_batch_number_with_metadata()
            .await?
            .context("all L1 batches with metadata disappeared")
            .map_err(Error::Storage)?;
        drop(storage);
        let last_correct_l1_batch = self
            .detect_reorg(earliest_l1_batch, diverged_l1_batch)
            .await?;
        tracing::info!("Reorg localized: last correct L1 batch is #{last_correct_l1_batch}");
        Err(Error::ReorgDetected(last_correct_l1_batch))
    }

    /// Compares hashes of the given local miniblock and the same miniblock from main node.
    async fn miniblock_hashes_match(
        &self,
        miniblock_number: MiniblockNumber,
    ) -> Result<bool, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::Storage)?;
        let local_hash = storage
            .blocks_dal()
            .get_miniblock_header(miniblock_number)
            .await?
            .with_context(|| {
                format!("Header does not exist for local miniblock #{miniblock_number}")
            })
            .map_err(Error::Storage)?
            .hash;
        drop(storage);

        let Some(remote_hash) = self.client.miniblock_hash(miniblock_number).await? else {
            // Due to reorg, locally we may be ahead of the main node.
            // Lack of the hash on the main node is treated as a hash match,
            // We need to wait for our knowledge of main node to catch up.
            return Ok(true);
        };

        if remote_hash != local_hash {
            tracing::warn!(
                "Reorg detected: local hash {local_hash:?} doesn't match the hash from \
                main node {remote_hash:?} (miniblock #{miniblock_number})"
            );
        }
        Ok(remote_hash == local_hash)
    }

    /// Compares root hashes of the latest local batch and of the same batch from the main node.
    async fn root_hashes_match(&self, l1_batch_number: L1BatchNumber) -> Result<bool, Error> {
        let mut storage = self.pool.access_storage().await.map_err(Error::Storage)?;
        let local_hash = storage
            .blocks_dal()
            .get_l1_batch_state_root(l1_batch_number)
            .await?
            .with_context(|| format!("Root hash does not exist for local batch #{l1_batch_number}"))
            .map_err(Error::Storage)?;
        drop(storage);

        let Some(remote_hash) = self.client.l1_batch_root_hash(l1_batch_number).await? else {
            tracing::info!("Remote L1 batch #{l1_batch_number} is missing");
            return Err(Error::NoRemoteL1Batch);
        };

        if remote_hash != local_hash {
            tracing::warn!(
                "Reorg detected: local root hash {local_hash:?} doesn't match the state hash from \
                main node {remote_hash:?} (L1 batch #{l1_batch_number})"
            );
        }
        Ok(remote_hash == local_hash)
    }

    /// Localizes a re-org: performs binary search to determine the last non-diverged block.
    async fn detect_reorg(
        &self,
        known_valid_l1_batch: L1BatchNumber,
        diverged_l1_batch: L1BatchNumber,
    ) -> Result<L1BatchNumber, Error> {
        // TODO (BFT-176, BFT-181): We have to look through the whole history, since batch status updater may mark
        //   a block as executed even if the state diverges for it.
        binary_search_with(
            known_valid_l1_batch.0,
            diverged_l1_batch.0,
            |number| async move {
                match self.root_hashes_match(L1BatchNumber(number)).await {
                    Err(Error::NoRemoteL1Batch) => Ok(true),
                    res => res,
                }
            },
        )
        .await
        .map(L1BatchNumber)
    }

    pub async fn run(mut self, mut stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        self.event_handler.initialize();
        while !*stop_receiver.borrow() {
            match self.run_inner(&mut stop_receiver).await {
                Ok(()) => continue,
                Err(err) if err.is_transient() => {
                    tracing::warn!("Following transient error occurred: {err}");
                    tracing::info!("Trying again after a delay");
                    tokio::time::sleep(self.sleep_interval).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        self.event_handler.start_shutting_down();
        tracing::info!("Shutting down reorg detector");
        Ok(())
    }

    async fn run_inner(&mut self, stop_receiver: &mut watch::Receiver<bool>) -> Result<(), Error> {
        let Some(earliest_l1_batch) =
            wait_for_l1_batch_with_metadata(&self.pool, self.sleep_interval, stop_receiver)
                .await
                .map_err(Error::Storage)?
        else {
            return Ok(()); // Stop signal received
        };
        tracing::debug!("Checking root hash match for earliest L1 batch #{earliest_l1_batch}");
        match self.root_hashes_match(earliest_l1_batch).await {
            Ok(true) => {}
            Ok(false) => return Err(Error::EarliestHashMismatch(earliest_l1_batch)),
            Err(Error::NoRemoteL1Batch) => {
                return Err(Error::EarliestL1BatchTruncated(earliest_l1_batch))
            }
            Err(err) => return Err(err),
        }
        while !*stop_receiver.borrow() {
            self.check_consistency().await?;
            tokio::time::sleep(self.sleep_interval).await;
        }
        Ok(())
    }
}
