//! Live Hyperliquid mainnet smoke test — `#[ignore]`-gated so it never runs in
//! normal `cargo test` (it hits the real network). Run it manually with:
//!
//!     cargo test --test hyperliquid_live -- --ignored
//!
//! It verifies the exact SDK message shapes the raptor parses: that within 60s
//! we receive at least one `Trades` message with a parseable `px`, and at least
//! one perp `ActiveAssetCtx` with parseable `funding` and `open_interest`.
#![cfg(feature = "hyperliquid")]

use std::str::FromStr;
use std::time::Duration;

use hyperliquid_rust_sdk::{AssetCtx, BaseUrl, InfoClient, Message, Subscription};
use rust_decimal::Decimal;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[tokio::test]
#[ignore = "hits Hyperliquid mainnet over the network; run with --ignored"]
async fn hyperliquid_live_trades_and_ctx_arrive() {
    let mut client = InfoClient::new(None, Some(BaseUrl::Mainnet))
        .await
        .expect("build InfoClient against Hyperliquid mainnet");

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    client
        .subscribe(Subscription::Trades { coin: "BTC".to_string() }, tx.clone())
        .await
        .expect("subscribe Trades{BTC}");
    client
        .subscribe(Subscription::ActiveAssetCtx { coin: "BTC".to_string() }, tx.clone())
        .await
        .expect("subscribe ActiveAssetCtx{BTC}");
    drop(tx);

    let mut got_trade = false;
    let mut got_ctx = false;

    // Bound the whole exchange to 60s.
    let _ = timeout(Duration::from_secs(60), async {
        while !(got_trade && got_ctx) {
            match rx.recv().await {
                Some(Message::Trades(trades)) => {
                    for t in trades.data {
                        assert!(
                            Decimal::from_str(&t.px).is_ok(),
                            "trade px should parse as Decimal, got {:?}",
                            t.px
                        );
                        got_trade = true;
                    }
                }
                Some(Message::ActiveAssetCtx(actx)) => {
                    if let AssetCtx::Perps(perp) = actx.data.ctx {
                        assert!(
                            Decimal::from_str(&perp.funding).is_ok(),
                            "funding should parse as Decimal, got {:?}",
                            perp.funding
                        );
                        assert!(
                            Decimal::from_str(&perp.open_interest).is_ok(),
                            "open_interest should parse as Decimal, got {:?}",
                            perp.open_interest
                        );
                        got_ctx = true;
                    }
                }
                Some(_) => {}
                None => break, // all senders dropped / socket closed
            }
        }
    })
    .await;

    assert!(got_trade, "expected ≥1 Trades message within 60s");
    assert!(got_ctx, "expected ≥1 perp ActiveAssetCtx message within 60s");
}
