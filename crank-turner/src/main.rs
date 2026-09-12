//! The crank turner daemon.
//!
//! Everything the binary does beyond wiring is in `daemon/`: [`daemon::args`]
//! is the command line, [`daemon::setup`] turns it into configuration, and
//! [`daemon::run`] is the timer loop. What is left here is the choice of
//! transport, which is the one decision that changes how the pieces connect.

mod daemon;

use anyhow::{Context, Result};
use clap::Parser;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::signer::EncodableKey;
use tracing::{info, warn};

use relay_crank_turner::{
    derive_ws_url, feed_channel, metrics, spawn_grpc_feed, spawn_submitter, spawn_ws_feed,
    CachedSource, GrpcFeedConfig, RpcSource, Turner, WatchFilter,
};

use daemon::args::{shellexpand, Args, Transport};
use daemon::run::run;
use daemon::setup::{
    cached_config, submitter_config, subscriptions, turner_config, watch_filter, with_local_sim,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let keeper = Keypair::read_from_file(shellexpand(&args.keypair))
        .map_err(|e| anyhow::anyhow!("read keypair {}: {e}", args.keypair))?;
    let filter = watch_filter(&args);
    let config = turner_config(&args, filter.clone(), keeper.pubkey())?;

    let metrics_port = args.metrics_port;
    tokio::spawn(async move {
        if let Err(err) = metrics::serve(metrics_port).await {
            warn!(error = %format!("{err:#}"), "metrics server stopped");
        }
    });

    let source = transport_source(&args, &filter)?;
    let submitter = spawn_submitter(std::sync::Arc::clone(&source), submitter_config(&args));
    run(
        Turner::new(source, keeper, config).with_submitter(submitter),
        &args,
    )
    .await
}

/// The account source the chosen transport produces, already wrapped in the
/// local simulator unless the operator opted out.
///
/// The two feed transports differ only in how the subscription is started;
/// both end in the same cache in front of the same RPC source.
fn transport_source(
    args: &Args,
    filter: &WatchFilter,
) -> Result<std::sync::Arc<dyn relay_crank_turner::ChainSource>> {
    let rpc = RpcSource::new(args.rpc_url.clone());
    let Transport::Rpc = args.transport else {
        let (sender, receiver) = feed_channel();
        match args.transport {
            Transport::Ws => {
                let ws_url = args
                    .ws_url
                    .clone()
                    .unwrap_or_else(|| derive_ws_url(&args.rpc_url));
                spawn_ws_feed(ws_url.clone(), subscriptions(args, filter), sender);
                info!(%ws_url, watched = args.watch_program.len(), "websocket subscriptions enabled");
            }
            Transport::Grpc => {
                let endpoint = args
                    .grpc_endpoint
                    .clone()
                    .context("--grpc-endpoint is required for the grpc transport")?;
                spawn_grpc_feed(
                    GrpcFeedConfig {
                        endpoint: endpoint.clone(),
                        x_token: args.grpc_x_token.clone(),
                    },
                    subscriptions(args, filter),
                    sender,
                );
                info!(%endpoint, watched = args.watch_program.len(), "yellowstone gRPC subscriptions enabled");
            }
            Transport::Rpc => unreachable!("handled above"),
        }
        return Ok(with_local_sim(
            CachedSource::new(rpc, receiver, cached_config(args)),
            args,
        ));
    };
    Ok(with_local_sim(rpc, args))
}
