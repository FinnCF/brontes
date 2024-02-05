use std::{collections::HashMap, sync::Arc};

use brontes_database::libmdbx::LibmdbxReader;
use brontes_types::{
    constants::{is_euro_stable, is_gold_stable, is_usd_stable},
    db::dex::PriceAt,
    mev::{AtomicArb, Bundle, MevType},
    normalized_actions::{Actions, NormalizedSwap},
    tree::BlockTree,
    ToFloatNearest, TxInfo,
};
use itertools::Itertools;
use malachite::{num::basic::traits::Zero, Rational};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use reth_primitives::Address;

use crate::{shared_utils::SharedInspectorUtils, BundleData, Inspector, Metadata};

pub struct AtomicArbInspector<'db, DB: LibmdbxReader> {
    inner: SharedInspectorUtils<'db, DB>,
}

impl<'db, DB: LibmdbxReader> AtomicArbInspector<'db, DB> {
    pub fn new(quote: Address, db: &'db DB) -> Self {
        Self { inner: SharedInspectorUtils::new(quote, db) }
    }
}

#[async_trait::async_trait]
impl<DB: LibmdbxReader> Inspector for AtomicArbInspector<'_, DB> {
    async fn process_tree(
        &self,
        tree: Arc<BlockTree<Actions>>,
        meta_data: Arc<Metadata>,
    ) -> Vec<Bundle> {
        let intersting_state = tree.collect_all(|node| {
            (
                node.data.is_swap() || node.data.is_transfer() || node.data.is_flash_loan(),
                node.subactions.iter().any(|action| {
                    action.is_swap() || action.is_transfer() || node.data.is_flash_loan()
                }),
            )
        });

        intersting_state
            .into_par_iter()
            .filter_map(|(tx, swaps)| {
                let info = tree.get_tx_info(tx)?;

                self.process_swaps(info, meta_data.clone(), vec![swaps])
            })
            .collect::<Vec<_>>()
    }
}

impl<DB: LibmdbxReader> AtomicArbInspector<'_, DB> {
    fn process_swaps(
        &self,
        info: TxInfo,
        metadata: Arc<Metadata>,
        searcher_actions: Vec<Vec<Actions>>,
    ) -> Option<Bundle> {
        let swaps = searcher_actions
            .iter()
            .flatten()
            .filter(|s| s.is_swap() || s.is_flash_loan())
            .flat_map(|s| match s.clone() {
                Actions::Swap(s) => vec![s],
                Actions::FlashLoan(f) => f
                    .child_actions
                    .into_iter()
                    .filter(|a| a.is_swap())
                    .map(|s| s.force_swap())
                    .collect_vec(),
                _ => vec![],
            })
            .collect_vec();

        let possible_arb_type = self.is_possible_arb(swaps)?;

        let profit = match possible_arb_type {
            AtomicArbitrage::LongTail => return None,
            AtomicArbitrage::Triangle => {
                self.process_triangle_arb(info, metadata.clone(), &searcher_actions)
            }
            AtomicArbitrage::CrossPair => {
                self.process_cross_pair_arb(info, metadata.clone(), &searcher_actions)
            }
        }?;

        let header = self.inner.build_bundle_header(
            &info,
            profit.to_float(),
            PriceAt::Average,
            &searcher_actions,
            &vec![info.gas_details],
            metadata,
            MevType::AtomicArb,
        );

        let swaps = searcher_actions
            .into_iter()
            .flatten()
            .filter(|actions| actions.is_swap())
            .map(|s| s.force_swap())
            .collect::<Vec<_>>();

        let backrun = AtomicArb { tx_hash: info.tx_hash, gas_details: info.gas_details, swaps };

        Some(Bundle { header, data: BundleData::AtomicArb(backrun) })
    }

    fn is_possible_arb(&self, swaps: Vec<NormalizedSwap>) -> Option<AtomicArbitrage> {
        // check to see if more than 1 swap
        if swaps.len() <= 1 {
            return Some(AtomicArbitrage::LongTail)
        } else if swaps.len() == 2 {
            let start = swaps[0].token_in.address;
            let end = swaps[1].token_out.address;

            if start == end && swaps[0].token_out.address == swaps[1].token_in.address {
                return Some(AtomicArbitrage::Triangle)
            } else {
                return Some(AtomicArbitrage::CrossPair)
            }
        } else {
            Some(identify_arb_sequence(swaps))
        }
    }

    fn process_triangle_arb(
        &self,
        tx_info: TxInfo,
        metadata: Arc<Metadata>,
        searcher_actions: &Vec<Vec<Actions>>,
    ) -> Option<Rational> {
        let rev_usd = self.inner.get_dex_revenue_usd(
            tx_info.tx_index,
            PriceAt::Average,
            &searcher_actions,
            metadata.clone(),
        )?;

        let gas_used = tx_info.gas_details.gas_paid();
        let gas_used_usd = metadata.get_gas_price_usd(gas_used);

        // Can change this later to check if people are subsidizing arbs to kill the
        // dry out the competition
        if &rev_usd - &gas_used_usd <= Rational::ZERO {
            return None
        } else {
            Some(rev_usd - &gas_used_usd)
        }
    }

    fn process_cross_pair_arb(
        &self,
        tx_info: TxInfo,
        metadata: Arc<Metadata>,
        searcher_actions: &Vec<Vec<Actions>>,
    ) -> Option<Rational> {
        let rev_usd = self.inner.get_dex_revenue_usd(
            tx_info.tx_index,
            PriceAt::Average,
            &searcher_actions,
            metadata.clone(),
        )?;

        let gas_used = tx_info.gas_details.gas_paid();
        let gas_used_usd = metadata.get_gas_price_usd(gas_used);

        // Can change this later to check if people are subsidizing arbs to kill the
        // dry out the competition
        if &rev_usd - &gas_used_usd <= Rational::ZERO {
            return None
        } else {
            Some(rev_usd - &gas_used_usd)
        }
    }
}

fn identify_arb_sequence(swaps: Vec<NormalizedSwap>) -> AtomicArbitrage {
    let start_token = swaps.first().unwrap().token_in.address;
    let end_token = swaps.last().unwrap().token_out.address;

    if start_token != end_token {
        return AtomicArbitrage::LongTail
    }

    let mut last_out = swaps.first().unwrap().token_out.address;

    for swap in swaps.iter().skip(1) {
        if swap.token_in.address != last_out {
            return AtomicArbitrage::CrossPair
        }
        last_out = swap.token_out.address;
    }

    AtomicArbitrage::Triangle
}

/// Represents the different types of atomic arb
/// A triangle arb is a simple arb that goes from token A -> B -> C -> A
/// A cross pair arb is a more complex arb that goes from token A -> B -> C -> A
enum AtomicArbitrage {
    LongTail,
    Triangle,
    CrossPair,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use brontes_types::constants::USDT_ADDRESS;
    use serial_test::serial;

    use crate::{
        test_utils::{InspectorTestUtils, InspectorTxRunConfig, USDC_ADDRESS},
        Inspectors,
    };

    #[tokio::test]
    #[serial]
    async fn test_backrun() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);

        let tx = hex!("76971a4f00a0a836322c9825b6edf06c8c49bf4261ef86fc88893154283a7124").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices()
            .with_expected_profit_usd(0.188588)
            .with_gas_paid_usd(71.632668);

        inspector_util.run_inspector(config, None).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_simple_triangular() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("67d9884157d495df4eaf24b0d65aeca38e1b5aeb79200d030e3bb4bd2cbdcf88").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices()
            .with_expected_profit_usd(311.18)
            .with_gas_paid_usd(91.51);

        inspector_util.run_inspector(config, None).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_uni_router() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("ac1127310fdec0b07e618407eabfb7cdf5ada81dc47e914c76fc759843346a0e").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_1_inch() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("3b6d8fcf36546e5d371b1b38f3a5beb02438dfa4d5a047c74884341c89286c3a").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_not_false_positive_hex_usdc() {
        let inspector_util = InspectorTestUtils::new(USDC_ADDRESS, 0.5);
        let tx = hex!("e4b8b358118daa26809a1ff77323d825664202c4f31a2afe923f3fe83d7eccc4").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.assert_no_mev(config).await.unwrap();
    }

    //TODO:
    #[tokio::test]
    #[serial]
    async fn test_cross_stable_arb() {
        let inspector_util = InspectorTestUtils::new(USDT_ADDRESS, 0.5);
        let tx = hex!("397c98efa1991e0384db16c56bd1693fb82addc7d932328941912afa8176cdb1").into();
        let config = InspectorTxRunConfig::new(Inspectors::AtomicArb)
            .with_mev_tx_hashes(vec![tx])
            .with_dex_prices();

        inspector_util.run_inspector(config, None).await.unwrap();
    }
}
