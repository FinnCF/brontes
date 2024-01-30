use std::sync::Arc;

use brontes_database::libmdbx::LibmdbxReader;
use brontes_types::{
    db::cex::CexExchange,
    mev::{Bundle, BundleData, CexDex, MevType, TokenProfit, TokenProfits},
    normalized_actions::{Actions, NormalizedSwap},
    pair::Pair,
    tree::{BlockTree, GasDetails},
    PriceKind, Root, ToFloatNearest, ToScaledRational,
};
use malachite::{
    num::basic::traits::{Two, Zero},
    Rational,
};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use reth_primitives::Address;
use tracing::{debug, error};

use crate::{shared_utils::SharedInspectorUtils, BundleHeader, Inspector, MetadataCombined};

pub struct CexDexInspector<'db, DB: LibmdbxReader> {
    inner:         SharedInspectorUtils<'db, DB>,
    cex_exchanges: Vec<CexExchange>,
}

impl<'db, DB: LibmdbxReader> CexDexInspector<'db, DB> {
    pub fn new(quote: Address, db: &'db DB, cex_exchanges: &Vec<CexExchange>) -> Self {
        Self {
            inner:         SharedInspectorUtils::new(quote, db),
            cex_exchanges: cex_exchanges.clone(),
        }
    }
}
//TODO: Support for multiple CEXs
//TODO: Start adding filtering by function sig in the tree. Like executeFFsYo
//TODO: If single swap with coinbase.transfer then flag token as missing cex
// price (addr + symbol + name) in a db table so we can fill in what is missing

#[async_trait::async_trait]
impl<DB: LibmdbxReader> Inspector for CexDexInspector<'_, DB> {
    async fn process_tree(
        &self,
        tree: Arc<BlockTree<Actions>>,
        meta_data: Arc<MetadataCombined>,
    ) -> Vec<Bundle> {
        // Get all normalized swaps
        let intersting_state = tree.collect_all(|node| {
            (node.data.is_swap(), node.subactions.iter().any(|action| action.is_swap()))
        });

        intersting_state
            .into_par_iter()
            .filter(|(_, swaps)| !swaps.is_empty())
            .filter_map(|(tx, swaps)| {
                let root = tree.get_root(tx)?;

                self.process_swaps(root, meta_data.clone(), swaps)
            })
            .collect::<Vec<_>>()
    }
}

impl<DB: LibmdbxReader> CexDexInspector<'_, DB> {
    fn process_swaps(
        &self,
        root: &Root<Actions>,
        metadata: Arc<MetadataCombined>,
        swaps: Vec<Actions>,
    ) -> Option<Bundle> {
        let tx_index = root.get_block_position();
        let gas_details = root.gas_details;
        let mev_contract = root.head.data.get_to_address();
        let eoa = root.head.address;

        let swaps_with_profit_by_exchange: Vec<(&NormalizedSwap, Vec<(CexExchange, Rational)>)> =
            swaps
                .iter()
                .filter_map(|action| {
                    let swap = action.force_swap_ref();

                    let cex_dex_opportunity =
                        self.detect_cex_dex_opportunity(swap, metadata.as_ref())?;

                    Some((swap, cex_dex_opportunity))
                })
                .collect();
        let possible_cex_dex =
            self.gas_accounting(swaps_with_profit_by_exchange, &gas_details, &metadata.eth_prices);

        let cex_dex = self.filter_possible_cex_dex(possible_cex_dex, root)?;

        let gas_finalized = metadata.get_gas_price_usd(gas_details.gas_paid());
        let deltas = self.inner.calculate_token_deltas(&vec![swaps.clone()]);

        let addr_usd_deltas =
            self.inner
                .usd_delta_by_address(tx_index, &deltas, metadata.clone(), true)?;

        let mev_profit_collector = self.inner.profit_collectors(&addr_usd_deltas);

        let token_profits = TokenProfits {
            profits: mev_profit_collector
                .iter()
                .filter_map(|address| deltas.get(address).map(|d| (address, d)))
                .flat_map(|(address, delta)| {
                    delta.iter().map(|(token, amount)| {
                        let usd_value = metadata
                            .cex_quotes
                            .get_quote(&Pair(*token, self.inner.quote), &CexExchange::Binance)
                            .unwrap_or_default()
                            .price
                            .1
                            .to_float()
                            * amount.clone().to_float();
                        TokenProfit {
                            profit_collector: *address,
                            token: *token,
                            amount: amount.clone().to_float(),
                            usd_value,
                        }
                    })
                })
                .collect(),
        };

        //TODO: Add clean bundle header contructor in shared utils
        let header = BundleHeader {
            tx_index: tx_index as u64,
            mev_profit_collector,
            tx_hash: root.tx_hash,
            mev_contract,
            eoa,
            block_number: metadata.block_num,
            mev_type: MevType::CexDex,
            profit_usd: 0.0,
            token_profits,
            bribe_usd: gas_finalized.to_float(),
        };

        Some(Bundle { header, data: cex_dex })
    }

    pub fn detect_cex_dex_opportunity(
        &self,
        swap: &NormalizedSwap,
        metadata: &MetadataCombined,
    ) -> Option<Vec<(CexExchange, Rational)>> {
        let cex_prices = self.cex_quotes_for_swap(swap, metadata)?;
        let dex_price = self.dex_price_post_fee(swap)?;

        let opportunities = cex_prices
            .into_iter()
            .map(|(exchange, price, is_direct_pair)| {
                self.profit_classifier(swap, &dex_price, (exchange, price, is_direct_pair))
            })
            .collect();

        Some(opportunities)
    }

    fn profit_classifier(
        &self,
        swap: &NormalizedSwap,
        dex_price: &Rational,
        exchange_cex_price: (CexExchange, Rational, bool),
    ) -> (CexExchange, Rational) {
        // It is the cex price - dex price because we are selling the token purchased on
        // the Dex on the Cex
        let delta_price = exchange_cex_price.1 - dex_price;

        //TODO: Remove once we have the new normalized swap
        // Calculate the potential profit
        let decimals_in = self
            .inner
            .db
            .try_get_token_decimals(swap.token_out)
            .unwrap()
            .unwrap();

        let sell_amount = swap.amount_out.to_scaled_rational(decimals_in);

        // Here we are calculating the profit of selling the token (purchased on the
        // Dex) on the Cex & accounting for trading fees TODO: Here we assume
        // taker fee, have to also account for maker fee
        if exchange_cex_price.2 {
            // Direct pair
            (
                exchange_cex_price.0,
                delta_price * &sell_amount - sell_amount * &exchange_cex_price.0.fees().1,
            )
        } else {
            // Indirect pair pays twice the fee
            (
                exchange_cex_price.0,
                delta_price * &sell_amount
                    - sell_amount * exchange_cex_price.0.fees().1 * Rational::TWO,
            )
        }
    }

    /// Gets the Cex quote for a Dex swap by exchange
    /// Retrieves CEX quotes for a DEX swap, grouped by exchange.
    /// If the quote is not found for a given exchange, it will try to find a
    /// quote via an intermediary token. Direct quotes are marked as
    /// `true`, intermediary quotes are marked as `false`. Which allows us to
    /// account for the additional fees.
    fn cex_quotes_for_swap(
        &self,
        swap: &NormalizedSwap,
        metadata: &MetadataCombined,
    ) -> Option<Vec<(CexExchange, Rational, bool)>> {
        let pair = Pair(swap.token_in, swap.token_out).ordered();
        let quotes = self
            .cex_exchanges
            .iter()
            .filter_map(|&exchange| {
                metadata
                    .db
                    .cex_quotes
                    .get_quote(&pair, &exchange)
                    .map(|cex_quote| (exchange, cex_quote.price.0, true))
                    .or_else(|| {
                        metadata
                            .db
                            .cex_quotes
                            .get_quote_via_intermediary(&pair, &exchange)
                            .map(|cex_quote| (exchange, cex_quote.price.0, false))
                    })
                    .or_else(|| {
                        debug!(
                            "No CEX quote found for pair: {}, {} at exchange: {:?}",
                            swap.token_in, swap.token_out, exchange
                        );
                        None
                    })
            })
            .collect::<Vec<_>>();

        if quotes.is_empty() {
            None
        } else {
            Some(quotes)
        }
    }

    fn dex_price_post_fee(&self, swap: &NormalizedSwap) -> Option<Rational> {
        //TODO: Prune this once will has added classifier based conversions
        let Ok(Some(decimals_in)) = self.inner.db.try_get_token_decimals(swap.token_in) else {
            error!(missing_token=?swap.token_in, "missing token in token to decimal map");
            return None
        };
        let Ok(Some(decimals_out)) = self.inner.db.try_get_token_decimals(swap.token_out) else {
            debug!(missing_token=?swap.token_out, "missing token out token to decimal map");
            return None
        };

        let adjusted_in = swap.amount_in.to_scaled_rational(decimals_in);
        let adjusted_out = swap.amount_out.to_scaled_rational(decimals_out);

        Some(adjusted_in / adjusted_out)
    }

    fn gas_accounting(
        &self,
        swaps_with_profit_by_exchange: Vec<(&NormalizedSwap, Vec<(CexExchange, Rational)>)>,
        gas_details: &GasDetails,
        eth_price: &Rational,
    ) -> PossibleCexDex {
        // Calculate the maximally profitable sequence of Cex arbs by picking the most
        // profitable exchange to execute the arb for each swap
        let max_profit_sequence: Vec<(NormalizedSwap, CexExchange, Rational)> =
            swaps_with_profit_by_exchange
                .into_iter()
                .filter_map(|(swap, net_profits_by_exchange)| {
                    net_profits_by_exchange
                        .into_iter()
                        .max_by(|(_, profit1), (_, profit2)| profit1.cmp(profit2))
                        .map(|(exchange, profit)| (swap.clone(), exchange, profit))
                })
                .collect();

        // Calculate total arbitrage profit before gas
        let total_arb_pre_gas: Rational = max_profit_sequence
            .iter()
            .map(|(_, _, profit)| profit)
            .sum();

        let gas_cost = Rational::from_unsigneds(gas_details.gas_paid(), 10u128.pow(18)) * eth_price;
        let pnl = total_arb_pre_gas - gas_cost;

        let swaps = max_profit_sequence
            .iter()
            .map(|(swap, ..)| swap.clone())
            .collect();
        let exchanges = max_profit_sequence
            .iter()
            .map(|(_, exchange, _)| *exchange)
            .collect();
        let profits_pre_gas = max_profit_sequence
            .iter()
            .map(|(_, _, profit)| profit.clone())
            .collect();

        PossibleCexDex { swaps, exchanges, profits_pre_gas, gas_details: gas_details.clone(), pnl }
    }

    fn filter_possible_cex_dex(
        &self,
        possible_cex_dex: PossibleCexDex,
        root: &Root<Actions>,
    ) -> Option<BundleData> {
        // Check if pnl is positive or a coinbase transfer is present
        let basic_condition = possible_cex_dex.pnl > Rational::ZERO
            && root.head.data.is_unclassified()
            || possible_cex_dex.gas_details.coinbase_transfer.is_some()
                && root.head.data.is_unclassified();

        let is_know_cex_dex_contract = if let Actions::Unclassified(data) = &root.head.data {
            data.is_cex_dex_call()
        } else {
            false
        };

        // Return Some(BundleData) if any of the conditions are met
        if basic_condition || is_know_cex_dex_contract {
            Some(possible_cex_dex.build_bundle_type(root))
        } else {
            None
        }
    }

    /// Calculates the rate for a given DEX swap
    #[allow(dead_code)]
    //TODO: implement as a method once we have the new normalized swap
    fn calculate_dex_rate(amount_in: Rational, amount_out: Rational) -> Rational {
        // Choose the calculation method based on your standard representation
        amount_in / amount_out
    }
}

pub struct PossibleCexDex {
    pub swaps:           Vec<NormalizedSwap>,
    pub exchanges:       Vec<CexExchange>,
    pub profits_pre_gas: Vec<Rational>,
    pub gas_details:     GasDetails,
    pub pnl:             Rational,
}

impl PossibleCexDex {
    //TODO: Build the bundle type & change cex dex type to contain cex-dex prices
    pub fn build_bundle_type(&self, root: &Root<Actions>) -> BundleData {
        BundleData::CexDex(CexDex {
            tx_hash:        root.tx_hash,
            gas_details:    self.gas_details.clone(),
            swaps:          self.swaps.clone(),
            prices_kind:    self
                .swaps
                .iter()
                .flat_map(|_s| vec![PriceKind::Dex, PriceKind::Cex])
                .collect(),
            prices_address: self
                .swaps
                .iter()
                .flat_map(|s| vec![s.token_in, s.token_out])
                .collect(),
            prices_price:   self
                .profits_pre_gas
                .iter()
                .flat_map(|profit| vec![profit.clone().to_float(), profit.clone().to_float()])
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        str::FromStr,
    };

    use alloy_primitives::{hex, B256, U256};
    use brontes_types::db::cex::{CexPriceMap, CexQuote};
    use malachite::num::arithmetic::traits::Reciprocal;
    use serial_test::serial;

    use super::*;
    use crate::{
        test_utils::{InspectorTestUtils, InspectorTxRunConfig, USDC_ADDRESS},
        Inspectors,
    };

    #[tokio::test]
    #[serial]
    async fn test_cex_dex() {
        // sold eth to buy usdc on chain
        let tx_hash =
            B256::from_str("0x21b129d221a4f169de0fc391fe0382dbde797b69300a9a68143487c54d620295")
                .unwrap();

        // reciprocal because we store the prices as usdc / eth due to pair ordering
        let eth_price = Rational::try_from_float_simplest(1665.81)
            .unwrap()
            .reciprocal();
        let eth_cex = Rational::try_from_float_simplest(1645.81)
            .unwrap()
            .reciprocal();

        let eth_usdc = Pair(
            hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").into(),
            hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").into(),
        );
        let mut cex_map = HashMap::new();
        cex_map.insert(
            eth_usdc.ordered(),
            vec![CexQuote {
                price: (eth_cex.clone(), eth_cex),
                token0: Address::new(hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")),
                ..Default::default()
            }],
        );

        let cex_quotes = CexPriceMap(cex_map);

        let metadata = MetadataCombined {
            dex_quotes: brontes_types::db::dex::DexQuotes(vec![Some({
                let mut map = HashMap::new();
                map.insert(eth_usdc, eth_price.clone());
                map
            })]),
            db:         brontes_types::db::metadata::MetadataNoDex {
                block_num: 18264694,
                block_hash: U256::from_be_bytes(hex!(
                    "57968198764731c3fcdb0caff812559ce5035aabade9e6bcb2d7fcee29616729"
                )),
                block_timestamp: 0,
                relay_timestamp: None,
                p2p_timestamp: None,
                proposer_fee_recipient: Some(
                    hex!("95222290DD7278Aa3Ddd389Cc1E1d165CC4BAfe5").into(),
                ),
                proposer_mev_reward: None,
                cex_quotes,
                eth_prices: eth_price.reciprocal(),
                private_flow: HashSet::new(),
            },
        };

        let test_utils = InspectorTestUtils::new(USDC_ADDRESS, 2.0);

        let config = InspectorTxRunConfig::new(Inspectors::CexDex)
            .with_metadata_override(metadata)
            .with_mev_tx_hashes(vec![tx_hash])
            .with_gas_paid_usd(79836.4183)
            .with_expected_profit_usd(21270.966);

        test_utils.run_inspector(config, None).await.unwrap();
    }
}
