// CLI
use clap::Parser;
use eyre::Result;
use tracing_subscriber::{filter::EnvFilter, prelude::*};

// Misc
use std::{sync::Arc, time::Duration};

// local utils
use stress4844::bundle_builders::construct_bundle;
use stress4844::mev_boost_tools::initialize_mev_boost;

#[derive(Debug, Parser)]
struct Opts {
    #[arg(default_value = "1", long)]
    /// The number of blocks to run the stress test for
    blocks: usize,

    #[arg(default_value = "100", long, short, value_parser = clap::value_parser!(u8).range(1..=100))]
    /// What % of the block to fill (0-100).
    fill_pct: u8,

    #[arg(default_value = "128", long, short)]
    chunk_size: usize,

    /// The HTTP RPC endpoint to submit the transactions to.
    #[arg(long, short, value_parser = http_provider)]
    rpc_url: String,

    /// The private key for the wallet you'll submit the stress test
    /// transactions with. MUST have enough ETH to cover for the gas.
    #[arg(long, short)]
    tx_signer: String,

    /// The private key for the full-block template bundle signer wallet.
    #[arg(long, short)]
    bundle_signer: String,

    #[arg(default_value = "100", long, short)]
    gas_price: U256,

    #[arg(long, short, value_parser = from_dec_str)]
    payment: U256,
}

fn from_dec_str(s: &str) -> Result<U256, String> {
    U256::from_dec_str(s).map_err(|e| e.to_string())
}

fn http_provider(s: &str) -> Result<String, String> {
    if s.starts_with("http://") || s.starts_with("https://") {
        Ok(s.to_string())
    } else {
        Err(format!("URL does not start with http(s): {s}",))
    }
}

// From: https://github.com/ethereum/go-ethereum/blob/c2e0abce2eedc1ba2a1b32c46fd07ef18a25354a/core/txpool/txpool.go#L44-L55
/// `TX_SLOT_SIZE` is used to calculate how many data slots a single transaction
/// takes up based on its size. The slots are used as DoS protection, ensuring
/// that validating a new transaction remains a constant operation (in reality
/// O(maxslots), where max slots are 4 currently).
const _TX_SLOT_SIZE: usize = 32 * 1024;

/// txMaxSize is the maximum size a single transaction can have. This field has
/// non-trivial consequences: larger transactions are significantly harder and
/// more expensive to propagate; larger transactions also take more resources
/// to validate whether they fit into the pool or not.
const _TX_MAX_SIZE: usize = 4 * _TX_SLOT_SIZE; // 128KB

/// 1 kilobyte = 1024 bytes
const KB: usize = 1024;

/// Arbitrarily chosen number to cover for nonce+from+to+gas price size in a serialized
/// transaction
const TRIM_BYTES: usize = 500;

/// Address of the following contract to allow for easy coinbase payments on Goerli.
///
/// contract CoinbasePayer {
///     receive() payable external {
///         payable(address(block.coinbase)).transfer(msg.value);
///     }
/// }
const COINBASE_PAYER_ADDR: &str = "0x060d6635bb76c71871f97C12f10Fa20BD8e87eC0";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let opts = Opts::parse();
    let payment = opts.payment;

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::new("stress4844=trace"))
        .init();

    let interval = Duration::from_secs(1);
    let rpc_url = opts.rpc_url;
    let tx_signer = opts.tx_signer.strip_prefix("0x").unwrap_or(&opts.tx_signer);

    let (address, chain_id, provider) = initialize_mev_boost(rpc_url, tx_signer);

    // TODO: Do we want this to be different per transaction?
    let receiver: Address = "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".parse()?;

    // `CHUNKS_SIZE` Kilobytes per transaction, shave off 500 bytes to leave room for
    // the other fields to be serialized.
    let chunk = opts.chunk_size * KB - TRIM_BYTES;

    // Sample junk data for the blob.
    let blob = rand::thread_rng()
        .sample_iter(Standard)
        .take(6 * 1024)
        .collect::<Vec<u8>>();

    // Craft the transaction.
    let tx = TransactionRequest::new()
        .chain_id(chain_id)
        .value(0)
        .from(address)
        .to(receiver)
        .data(blob)
        .gas_price(opts.gas_price);

    let mut bundle = construct_bundle(
        provider.clone(),
        &tx,
        block.gas_limit,
        opts.fill_pct,
        nonce,
        payment,
    )
    .await?;

    // on every block try to get the bundle in
    let mut block_sub = provider.watch_blocks().await?;
    tracing::info!("subscribed to blocks - waiting for next");
    while block_sub.next().await.is_some() {
        let block_number = provider.get_block_number().await?;

        let span = tracing::trace_span!("submit-bundle", block = block_number.as_u64());
        let _enter = span.enter();

        bundle = bundle
            .set_block(block_number + 1)
            .set_simulation_block(block_number)
            .set_simulation_timestamp(0);
        tracing::debug!("bundle target block {:?}", block_number + 1);

        let pending_bundle = provider.inner().send_bundle(&bundle).await?;
        match pending_bundle.await {
            Ok(bundle_hash) => {
                // TODO: Can we log more info from the Flashbots API?
                tracing::info!("bundle included! hash: {:?}", bundle_hash);
                let nonce = provider
                    .get_transaction_count(address, Some(BlockNumber::Pending.into()))
                    .await?;
                tracing::debug!("signing new bundle for next block (new nonce: {})", nonce);
                bundle = construct_bundle(
                    provider.clone(),
                    &tx,
                    block.gas_limit,
                    opts.fill_pct,
                    nonce,
                    payment,
                )
                .await?;
            }
            Err(err) => {
                tracing::error!("{}. Retrying.", err);
            }
        }
    }

    tracing::debug!("Done! End Block: {}", provider.get_block_number().await?);

    Ok(())
}
