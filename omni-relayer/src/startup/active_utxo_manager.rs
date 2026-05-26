use std::{sync::Arc, time::Duration};

use anyhow::Result;
use near_bridge_client::TransactionOptions;
use omni_connector::OmniConnector;
use omni_types::ChainKind;
use tracing::{info, warn};

use crate::{
    config::{ActiveUtxoManagement, LargeMerge, SmallMerge},
    utils,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeAction<'a> {
    None,
    Small(&'a SmallMerge),
    Large(&'a LargeMerge),
}

/// Pure decision: given the current UTXO count, the sum of the top
/// `large.top_n_window` balances (or `None` if there were fewer UTXOs than the
/// window), and both configs, decide what to run this tick.
///
/// Large-merge wins ties: when both conditions fire, we skip small and let it
/// run on a later tick.
fn decide_merge_action<'a>(
    utxo_count: u32,
    top_n_sum: Option<u64>,
    small: Option<&'a SmallMerge>,
    large: Option<&'a LargeMerge>,
) -> MergeAction<'a> {
    if let (Some(cfg), Some(sum)) = (large, top_n_sum) {
        if sum < cfg.top_n_sum_threshold {
            return MergeAction::Large(cfg);
        }
    }
    if let Some(cfg) = small {
        if utxo_count > cfg.utxo_count_threshold {
            return MergeAction::Small(cfg);
        }
    }
    MergeAction::None
}

/// Sum the top `window` entries from `balances`. Returns `None` when there are
/// strictly fewer entries than the window — the large-merge gate is meaningful
/// only when the window is fully populated.
fn top_n_sum(balances: &mut [u64], window: u32) -> Option<u64> {
    let window = usize::try_from(window).ok()?;
    if balances.len() < window || window == 0 {
        return None;
    }
    // Partial sort: place the `window` largest at the front in O(n).
    balances.select_nth_unstable_by(window - 1, |a, b| b.cmp(a));
    Some(
        balances[..window]
            .iter()
            .fold(0u64, |acc, b| acc.saturating_add(*b)),
    )
}

pub async fn start_active_utxo_manager(
    settings: ActiveUtxoManagement,
    chain: ChainKind,
    omni_connector: Arc<OmniConnector>,
    near_nonce: Arc<utils::nonce::NonceManager>,
) -> Result<()> {
    let interval = Duration::from_secs(settings.polling_interval_secs);

    info!(
        "Starting active UTXO manager for {chain:?} (small={}, large={}, interval={}s)",
        settings.small.is_some(),
        settings.large.is_some(),
        settings.polling_interval_secs,
    );

    loop {
        tokio::time::sleep(interval).await;

        let near_bridge_client = match omni_connector.near_bridge_client() {
            Ok(client) => client,
            Err(err) => {
                warn!("Active UTXO manager: NEAR bridge client unavailable for {chain:?}: {err:?}");
                continue;
            }
        };

        // If large-merge is configured we need the full UTXO map to compute
        // the top-N sum; otherwise the cheap count call is enough.
        let (utxo_count, top_sum) = if let Some(large) = settings.large.as_ref() {
            let utxos = match near_bridge_client.get_utxos(chain).await {
                Ok(u) => u,
                Err(err) => {
                    warn!("Active UTXO manager: failed to fetch UTXOs for {chain:?}: {err:?}");
                    continue;
                }
            };
            let count = u32::try_from(utxos.len()).unwrap_or(u32::MAX);
            let mut balances: Vec<u64> = utxos.values().map(|u| u.balance).collect();
            let sum = top_n_sum(&mut balances, large.top_n_window);
            (count, sum)
        } else {
            match near_bridge_client.get_utxo_num(chain).await {
                Ok(n) => (n, None),
                Err(err) => {
                    warn!("Active UTXO manager: failed to fetch UTXO count for {chain:?}: {err:?}");
                    continue;
                }
            }
        };

        let action = decide_merge_action(
            utxo_count,
            top_sum,
            settings.small.as_ref(),
            settings.large.as_ref(),
        );

        let (merge_largest, fee_rate, max_input_number) = match action {
            MergeAction::None => {
                info!(
                    "Active UTXO manager: {chain:?} nothing to do (utxos={utxo_count}, top_n_sum={top_sum:?})"
                );
                continue;
            }
            MergeAction::Small(cfg) => {
                info!(
                    "Active UTXO manager: {chain:?} small-merge fires (utxos={utxo_count} > {})",
                    cfg.utxo_count_threshold
                );
                (false, cfg.fixed_fee_rate, cfg.max_input_number)
            }
            MergeAction::Large(cfg) => {
                info!(
                    "Active UTXO manager: {chain:?} large-merge fires (top_n_sum={top_sum:?} < {})",
                    cfg.top_n_sum_threshold
                );
                (true, cfg.fixed_fee_rate, cfg.max_input_number)
            }
        };

        let nonce = match near_nonce.reserve_nonce() {
            Ok(nonce) => Some(nonce),
            Err(err) => {
                warn!("Active UTXO manager: failed to reserve nonce for {chain:?}: {err:?}");
                continue;
            }
        };

        match omni_connector
            .active_utxo_management(
                chain,
                fee_rate,
                max_input_number,
                merge_largest,
                TransactionOptions {
                    nonce,
                    wait_until: near_primitives::views::TxExecutionStatus::Final,
                    wait_final_outcome_timeout_sec: None,
                },
            )
            .await
        {
            Ok(tx_hash) => {
                info!(
                    "Active UTXO manager: submitted active_utxo_management for {chain:?} (merge_largest={merge_largest}): {tx_hash}"
                );
            }
            Err(err) => {
                warn!("Active UTXO manager: active_utxo_management failed for {chain:?}: {err:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(threshold: u32) -> SmallMerge {
        SmallMerge {
            utxo_count_threshold: threshold,
            fixed_fee_rate: None,
            max_input_number: None,
        }
    }

    fn large(window: u32, sum_threshold: u64) -> LargeMerge {
        LargeMerge {
            top_n_window: window,
            top_n_sum_threshold: sum_threshold,
            fixed_fee_rate: None,
            max_input_number: None,
        }
    }

    #[test]
    fn decide_nothing_when_no_config() {
        let action = decide_merge_action(10_000, Some(1), None, None);
        assert_eq!(action, MergeAction::None);
    }

    #[test]
    fn decide_small_fires_when_count_above_threshold() {
        let s = small(5000);
        let action = decide_merge_action(5001, None, Some(&s), None);
        assert!(matches!(action, MergeAction::Small(_)));
    }

    #[test]
    fn decide_small_skipped_at_exact_threshold() {
        let s = small(5000);
        let action = decide_merge_action(5000, None, Some(&s), None);
        assert_eq!(action, MergeAction::None);
    }

    #[test]
    fn decide_large_fires_when_top_sum_below_threshold() {
        let l = large(20, 1_500_000_000);
        let action = decide_merge_action(100, Some(1_000_000_000), None, Some(&l));
        assert!(matches!(action, MergeAction::Large(_)));
    }

    #[test]
    fn decide_large_skipped_when_top_sum_at_threshold() {
        let l = large(20, 1_500_000_000);
        let action = decide_merge_action(100, Some(1_500_000_000), None, Some(&l));
        assert_eq!(action, MergeAction::None);
    }

    #[test]
    fn decide_large_skipped_when_window_underfilled() {
        // top_n_sum is None when pool is smaller than the window.
        let l = large(20, 1_500_000_000);
        let action = decide_merge_action(5, None, None, Some(&l));
        assert_eq!(action, MergeAction::None);
    }

    #[test]
    fn decide_large_wins_when_both_fire() {
        let s = small(5000);
        let l = large(20, 1_500_000_000);
        let action = decide_merge_action(10_000, Some(1_000_000_000), Some(&s), Some(&l));
        assert!(matches!(action, MergeAction::Large(_)));
    }

    #[test]
    fn decide_small_runs_when_only_small_fires() {
        let s = small(5000);
        let l = large(20, 1_500_000_000);
        // Top-N sum is healthy, but pool is large — small-merge takes over.
        let action = decide_merge_action(10_000, Some(2_000_000_000), Some(&s), Some(&l));
        assert!(matches!(action, MergeAction::Small(_)));
    }

    #[test]
    fn top_n_sum_returns_none_when_pool_smaller_than_window() {
        let mut balances = vec![10_u64, 20, 30];
        assert_eq!(top_n_sum(&mut balances, 5), None);
    }

    #[test]
    fn top_n_sum_picks_largest_balances() {
        let mut balances = vec![5_u64, 1, 9, 3, 7, 2, 8];
        // Top 3: 9 + 8 + 7 = 24
        assert_eq!(top_n_sum(&mut balances, 3), Some(24));
    }

    #[test]
    fn top_n_sum_saturates_on_overflow() {
        let mut balances = vec![u64::MAX, u64::MAX, 1];
        assert_eq!(top_n_sum(&mut balances, 3), Some(u64::MAX));
    }
}
