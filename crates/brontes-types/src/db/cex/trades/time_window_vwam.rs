use std::{
    cmp::{max, min},
    f64::consts::E,
    ops::Mul,
};

use ahash::HashSetExt;
use alloy_primitives::{Address, FixedBytes};
use malachite::{
    num::basic::traits::{One, Zero},
    Rational,
};
use tracing::trace;

use super::{
    config::CexDexTradeConfig, utils::{log_insufficient_trade_volume, log_missing_trade_data, PairTradeWalker}, CexTrades
};
use crate::{
    constants::{USDC_ADDRESS, USDT_ADDRESS},
    db::cex::{CexExchange, CommodityClass},
    display::utils::format_etherscan_url,
    normalized_actions::NormalizedSwap,
    pair::Pair,
    FastHashMap, FastHashSet,
};

const PRE_DECAY: f64 = -0.0000005;
const POST_DECAY: f64 = -0.0000002;

const START_POST_TIME_US: u64 = 50_000;
const START_PRE_TIME_US: u64 = 50_000;

const PRE_SCALING_DIFF: u64 = 300_000;
const TIME_STEP: u64 = 10_000;

#[derive(Debug, Clone, Default)]
pub struct ExchangePath {
    pub price_maker:      Rational,
    pub price_taker:      Rational,
    pub volume:           Rational,
    // window results
    pub final_start_time: u64,
    pub final_end_time:   u64,
}

#[derive(Debug, Clone, Default)]
pub struct WindowExchangePrice {
    /// The price & volume of each exchange
    pub exchange_price_with_volume_direct: FastHashMap<CexExchange, ExchangePath>,
    /// the pairs that were traded through in order to get this price.
    /// in the case of a intermediary, this will be 2, otherwise, 1
    pub pairs: Vec<Pair>,
    /// Global Exchange Price
    pub global: ExchangePath,
}

impl Mul for WindowExchangePrice {
    type Output = WindowExchangePrice;

    fn mul(mut self, mut rhs: Self) -> Self::Output {
        self.exchange_price_with_volume_direct = self
            .exchange_price_with_volume_direct
            .into_iter()
            .filter_map(|(exchange, mut first_leg)| {
                let second_leg = rhs.exchange_price_with_volume_direct.remove(&exchange)?;
                first_leg.price_maker *= second_leg.price_maker;
                first_leg.price_taker *= second_leg.price_taker;

                first_leg.final_start_time =
                    min(first_leg.final_start_time, second_leg.final_start_time);

                first_leg.final_end_time = max(first_leg.final_end_time, second_leg.final_end_time);

                Some((exchange, first_leg))
            })
            .collect();

        self.pairs.extend(rhs.pairs);

        self.global.final_start_time =
            min(self.global.final_start_time, rhs.global.final_start_time);
        self.global.final_end_time = max(self.global.final_end_time, rhs.global.final_end_time);

        self.global.price_maker *= rhs.global.price_maker;
        self.global.price_taker *= rhs.global.price_taker;

        self
    }
}

// trades sorted by time-stamp with the index to block time-stamp closest to the
// block_number
pub struct TimeWindowTrades<'a> {
    pub trades: FastHashMap<&'a CexExchange, FastHashMap<&'a Pair, (usize, &'a Vec<CexTrades>)>>,
    pub intermediaries: FastHashSet<Address>,
}

impl<'a> TimeWindowTrades<'a> {
    pub fn new_from_cex_trade_map(
        trade_map: &'a FastHashMap<CexExchange, FastHashMap<Pair, Vec<CexTrades>>>,
        block_timestamp: u64,
        exchanges: &'a [CexExchange],
        pair: Pair,
    ) -> Self {
        let intermediaries = Self::calculate_intermediary_addresses(trade_map, exchanges, &pair);

        let map = trade_map
            .iter()
            .filter_map(|(ex, pairs)| {
                if !exchanges.contains(ex) || pair.0 == pair.1 {
                    return None
                }

                Some((
                    ex,
                    pairs
                        .iter()
                        .filter_map(|(ex_pair, trades)| {
                            if (ex_pair == &pair || ex_pair == &pair.flip())
                                || (ex_pair.0 == pair.0 && intermediaries.contains(&ex_pair.1))
                                || (ex_pair.1 == pair.0 && intermediaries.contains(&ex_pair.0))
                                || (ex_pair.0 == pair.1 && intermediaries.contains(&ex_pair.1))
                                || (ex_pair.1 == pair.1 && intermediaries.contains(&ex_pair.0))
                            {
                                let idx = trades
                                    .partition_point(|trades| trades.timestamp < block_timestamp);
                                Some((ex_pair, (idx, trades)))
                            } else {
                                None
                            }
                        })
                        .collect(),
                ))
            })
            .collect::<FastHashMap<&CexExchange, FastHashMap<&Pair, (usize, &Vec<CexTrades>)>>>();

        Self { trades: map, intermediaries }
    }

    pub(crate) fn get_price(
        &self,
        config: CexDexTradeConfig,
        exchanges: &[CexExchange],
        pair: Pair,
        volume: &Rational,
        timestamp: u64,
        bypass_vol: bool,
        dex_swap: &NormalizedSwap,
        tx_hash: FixedBytes<32>,
    ) -> Option<WindowExchangePrice> {
        if pair.0 == pair.1 {
            return Some(WindowExchangePrice::default())
        }

        let res = self
            .get_vwap_price(
                config, exchanges, pair, volume, timestamp, bypass_vol, dex_swap, tx_hash,
            )
            .or_else(|| {
                self.get_vwap_price_via_intermediary(
                    config, exchanges, &pair, volume, timestamp, bypass_vol, dex_swap, tx_hash,
                )
            });

        if res.is_none() {
            tracing::debug!(target: "brontes_types::db::cex::time_window_vwam", ?pair, "No price VMAP found for {}-{} in time window.\n Tx: {}", dex_swap.token_in.symbol, dex_swap.token_out.symbol, format_etherscan_url(&tx_hash));
        }

        res
    }

    fn get_vwap_price_via_intermediary(
        &self,
        config: CexDexTradeConfig,
        exchanges: &[CexExchange],
        pair: &Pair,
        volume: &Rational,
        block_timestamp: u64,
        bypass_vol: bool,
        dex_swap: &NormalizedSwap,
        tx_hash: FixedBytes<32>,
    ) -> Option<WindowExchangePrice> {
        self.intermediaries
            .iter()
            .filter_map(|intermediary| {
                trace!(target: "brontes_types::db::cex::time_window_vwam", ?intermediary, "trying intermediary");

                let pair0 = Pair(pair.0, *intermediary);
                let pair1 = Pair(*intermediary, pair.1);

                let mut bypass_intermediary_vol = false;

                // bypass volume requirements for stable pairs
                if pair0.0 == USDC_ADDRESS && pair0.1 == USDT_ADDRESS
                || pair0.0 == USDT_ADDRESS && pair0.1 == USDC_ADDRESS {
                    bypass_intermediary_vol = true;
                }

                tracing::debug!(target: "brontes_types::db::cex::time_window_vwam", ?pair, ?intermediary, ?volume, "trying via intermediary");
                let first_leg = self.get_vwap_price(
                    config,
                    exchanges,
                    pair0,
                    volume,
                    block_timestamp,
                    bypass_vol || bypass_intermediary_vol,
                    dex_swap,
                    tx_hash,
                )?;

                // Volume of second leg
                let second_leg_volume = &first_leg.global.price_maker * volume;

                bypass_intermediary_vol = false;
                if pair1.0 == USDT_ADDRESS && pair1.1 == USDC_ADDRESS
                || pair1.0 == USDC_ADDRESS && pair1.1 == USDT_ADDRESS{
                    bypass_intermediary_vol = true;
                }

                let second_leg = self.get_vwap_price(
                    config,
                    exchanges,
                    pair1,
                    &second_leg_volume,
                    block_timestamp,
                    bypass_vol || bypass_intermediary_vol,
                    dex_swap,
                    tx_hash,
                )?;

                let price = first_leg * second_leg;


                Some(price)
            })
            .max_by_key(|a| a.global.price_maker.clone())
    }

    #[allow(clippy::type_complexity)]
    /// Calculates the Volume Weighted Markout over a dynamic time window.
    ///
    /// This function adjusts the time window dynamically around a given block
    /// time to achieve a sufficient volume of trades for analysis. The
    /// initial time window is set to [-0.5, +2] (relative to
    /// the block time). If the volume is deemed insufficient within this
    /// window, the function extends the post-block window by increments of
    /// 0.1 up to +3. If still insufficient, it then extends the
    /// pre-block window by increments of 0.1 up to -2, while also allowing the
    /// post-block window to increment up to +4. If the volume remains
    /// insufficient, the post-block window may be extended further up to
    /// +5, and the pre-block window to -3.

    /// ## Execution Risk
    /// - **Risk of Price Movements**: Extending the time window increases the
    ///   risk of significant market condition changes that could negatively
    ///   impact arbitrage outcomes.
    ///
    /// ## Bi-Exponential Decay Function
    /// A bi-exponential decay function weights the trades based on their timing
    /// relative to the block time, skewing the weights to favor post-block
    /// trades to account for the certainty in DEX executions. The weight
    /// \(W(t)\) for a trade at time \(t\) is defined as follows:
    ///
    /// If t < BlockTime:  W(t) = exp(-lambda_pre * (BlockTime - t))
    /// If t >= BlockTime: W(t) = exp(-lambda_post * (t - BlockTime))
    ///
    /// Where:
    /// - `t`: timestamp of each trade.
    /// - `BlockTime`: time the block was first seen on the peer-to-peer
    ///   network.
    /// - `lambda_pre` and `lambda_post`: decay rates before and after the block
    ///   time, respectively.
    ///
    /// ## Adjusted Volume Weighted Average Price (VWAP)
    /// The Adjusted VWAP is calculated by integrating both the volume and the
    /// timing weights into the VWAP calculation:
    ///
    /// AdjustedVWAP = (Sum of (Price_i * Volume_i * TimingWeight_i)) / (Sum of
    /// (Volume_i * TimingWeight_i))

    //TODO: This currently expands the time window if the global volume is not met.
    //which means that each exchange is not actually expanded to the point of the
    // full arbitrage volume. We should probably redesign this later on to
    // improve upon this because that feels a bit weird.
    fn get_vwap_price(
        &self,
        config: CexDexTradeConfig,
        exchanges: &[CexExchange],
        pair: Pair,
        vol: &Rational,
        block_timestamp: u64,
        bypass_vol: bool,
        dex_swap: &NormalizedSwap,
        tx_hash: FixedBytes<32>,
    ) -> Option<WindowExchangePrice> {
        let trade_data = self.get_trades(exchanges, pair, dex_swap, tx_hash)?;

        let mut walker = PairTradeWalker::new(
            trade_data.trades,
            trade_data.indices,
            block_timestamp - START_PRE_TIME_US,
            block_timestamp + START_POST_TIME_US,
        );

        let mut trade_volume_global = Rational::ZERO;
        let mut exchange_vxp = FastHashMap::default();

        while trade_volume_global.le(vol) {
            let trades = walker.get_trades_for_window();
            for trade in trades {
                let trade = trade.get();
                let (m_fee, t_fee) = trade.exchange.fees(&pair, &CommodityClass::Spot);
                let weight = calculate_weight(block_timestamp, trade.timestamp);

                let (
                    vxp_maker,
                    vxp_taker,
                    trade_volume_weight,
                    trade_volume_ex,
                    start_time,
                    end_time,
                ) = exchange_vxp.entry(trade.exchange).or_insert((
                    Rational::ZERO,
                    Rational::ZERO,
                    Rational::ZERO,
                    Rational::ZERO,
                    0u64,
                    0u64,
                ));

                *vxp_maker += (&adjusted_trade.price * (Rational::ONE - m_fee))
                    * &adjusted_trade.amount
                    * &weight;
                *vxp_taker += (&adjusted_trade.price * (Rational::ONE - t_fee))
                    * &adjusted_trade.amount
                    * &weight;
                *trade_volume_weight += &adjusted_trade.amount * weight;
                *trade_volume_ex += &adjusted_trade.amount;
                trade_volume_global += &adjusted_trade.amount;

                *start_time = walker.min_timestamp;
                *end_time = walker.max_timestamp;
            }

            if walker.get_min_time_delta(block_timestamp) >= config.time_window_before_us
                || walker.get_max_time_delta(block_timestamp) >= config.time_window_after_us
            {
                break
            }

            let min_expand = (walker.get_max_time_delta(block_timestamp) >= PRE_SCALING_DIFF)
                .then_some(TIME_STEP)
                .unwrap_or_default();

            walker.expand_time_bounds(min_expand, TIME_STEP);
        }

        if &trade_volume_global < vol && !bypass_vol {
            log_insufficient_trade_volume(
                pair,
                dex_swap,
                &tx_hash,
                trade_volume_global,
                vol.clone(),
            );
            return None
        }

        let mut per_exchange_price = FastHashMap::default();

        let mut global_maker = Rational::ZERO;
        let mut global_taker = Rational::ZERO;

        let mut global_start_time = u64::MAX;
        let mut global_end_time = 0;

        for (ex, (vxp_maker, vxp_taker, trade_vol_weight, trade_vol, start_time, end_time)) in
            exchange_vxp
        {
            if trade_vol_weight == Rational::ZERO {
                continue
            }
            let maker_price = vxp_maker / &trade_vol_weight;
            let taker_price = vxp_taker / &trade_vol_weight;

            global_maker += &maker_price * &trade_vol;
            global_taker += &taker_price * &trade_vol;

            let exchange_price = ExchangePath {
                volume:           trade_vol.clone(),
                price_maker:      maker_price,
                price_taker:      taker_price,
                final_end_time:   end_time,
                final_start_time: start_time,
            };

            global_start_time = min(global_start_time, start_time);
            global_end_time = max(global_end_time, end_time);

            per_exchange_price.insert(ex, exchange_price);
        }

        if global_start_time == u64::MAX {
            global_start_time = 0;
        }

        if trade_volume_global == Rational::ZERO {
            log_insufficient_trade_volume(
                pair,
                dex_swap,
                &tx_hash,
                trade_volume_global,
                vol.clone(),
            );
            return None
        }

        let global_maker = global_maker / &trade_volume_global;
        let global_taker = global_taker / &trade_volume_global;

        let global = ExchangePath {
            volume:           trade_volume_global,
            price_maker:      global_maker,
            price_taker:      global_taker,
            final_start_time: global_start_time,
            final_end_time:   global_end_time,
        };

        let window_exchange_prices = WindowExchangePrice {
            exchange_price_with_volume_direct: per_exchange_price,
            global,
            pairs: vec![pair],
        };

        Some(window_exchange_prices)
    }

    pub fn get_trades(
        &'a self,
        exchanges: &[CexExchange],
        pair: Pair,
        dex_swap: &NormalizedSwap,
        tx_hash: FixedBytes<32>,
    ) -> Option<TradeData<'a>> {
        let (mut indices, mut trades) = self.query_trades(exchanges, &pair);

        if trades.iter().map(|(_, t)| t.len()).sum::<usize>() == 0 {
            let flipped_pair = pair.flip();
            (indices, trades) = self.query_trades(exchanges, &flipped_pair);

            if trades.iter().map(|(_, t)| t.len()).sum::<usize>() != 0 {
                trace!(
                    target: "brontes_types::db::cex::time_window_vwam",
                    trade_qty = %trades.len(),
                    "have trades (flipped pair)"
                );
                for (_, trades) in &trades {
                    trace!(
                    target: "brontes_types::db::cex::time_window_vwam",
                        trade_qty = %trades.len(),
                        "have trades inner(flipped)"
                    );
                }
                return Some(TradeData { indices, trades, direction: Direction::Buy })
            } else {
                log_missing_trade_data(dex_swap, &tx_hash);
                return None
            }
        }

        trace!(
            target: "brontes_types::db::cex::time_window_vwam",
            trade_qty = %trades.len(),
            "have trades"
        );
        for (_, trades) in &trades {
            trace!(
                target: "brontes_types::db::cex::time_window_vwam",
                trade_qty = %trades.len(),
                "have trades inner"
            );
        }

        Some(TradeData { indices, trades, direction: Direction::Sell })
    }

    fn query_trades(
        &'a self,
        exchanges: &[CexExchange],
        pair: &Pair,
    ) -> (FastHashMap<CexExchange, (usize, usize)>, Vec<(CexExchange, &'a Vec<CexTrades>)>) {
        self.trades
            .iter()
            .filter(|(e, _)| exchanges.contains(e))
            .filter_map(|(exchange, pairs)| Some((**exchange, pairs.get(pair)?)))
            .filter(|(_, (_, v))| !v.is_empty())
            .map(|(ex, (idx, trades))| ((ex, (idx.saturating_sub(1), *idx)), (ex, *trades)))
            .unzip()
    }

    fn calculate_intermediary_addresses(
        trade_map: &FastHashMap<CexExchange, FastHashMap<Pair, Vec<CexTrades>>>,
        exchanges: &[CexExchange],
        pair: &Pair,
    ) -> FastHashSet<Address> {
        let (token_a, token_b) = (pair.0, pair.1);
        let mut connected_to_a = FastHashSet::new();
        let mut connected_to_b = FastHashSet::new();

        trade_map
            .iter()
            .filter(|(exchange, _)| exchanges.contains(exchange))
            .flat_map(|(_, pairs)| pairs.keys())
            .for_each(|trade_pair| {
                if trade_pair.0 == token_a {
                    connected_to_a.insert(trade_pair.1);
                } else if trade_pair.1 == token_a {
                    connected_to_a.insert(trade_pair.0);
                }

                if trade_pair.0 == token_b {
                    connected_to_b.insert(trade_pair.1);
                } else if trade_pair.1 == token_b {
                    connected_to_b.insert(trade_pair.0);
                }
            });

        connected_to_a
            .intersection(&connected_to_b)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Buy,
    Sell,
}

#[derive(Debug)]
pub struct TradeData<'a> {
    pub indices:   FastHashMap<CexExchange, (usize, usize)>,
    pub trades:    Vec<(CexExchange, &'a Vec<CexTrades>)>,
    pub direction: Direction,
}

/// Calculates the weight for a trade using a bi-exponential decay function
/// based on its timestamp relative to a block time.
///
/// This function is designed to account for the risk associated with the timing
/// of trades in relation to block times in the context of cex-dex
/// arbitrage. This assumption underpins our pricing model: trades that
/// occur further from the block time are presumed to carry higher uncertainty
/// and an increased risk of adverse market conditions potentially impacting
/// arbitrage outcomes. Accordingly, the decay rates (`PRE_DECAY` for pre-block
/// and `POST_DECAY` for post-block) adjust the weight assigned to each trade
/// based on its temporal proximity to the block time.
///
/// Trades after the block are assumed to be generally preferred by arbitrageurs
/// as they have confirmation that their DEX swap is executed. However, this
/// preference can vary for less competitive pairs where the opportunity and
/// timing of execution might differ.
///
/// # Parameters
/// - `block_time`: The timestamp of the block as seen first on the peer-to-peer
///   network.
/// - `trade_time`: The timestamp of the trade to be weighted.
///
/// # Returns
/// Returns a `Rational` representing the calculated weight for the trade. The
/// weight is determined by:
/// - `exp(-PRE_DECAY * (block_time - trade_time))` for trades before the block
///   time.
/// - `exp(-POST_DECAY * (trade_time - block_time))` for trades after the block
///   time.

fn calculate_weight(block_time: u64, trade_time: u64) -> Rational {
    let pre = trade_time < block_time;

    Rational::try_from_float_simplest(if pre {
        E.powf(PRE_DECAY * (block_time - trade_time) as f64)
    } else {
        E.powf(POST_DECAY * (trade_time - block_time) as f64)
    })
    .unwrap()
}
