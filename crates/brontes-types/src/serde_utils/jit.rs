use ::serde::ser::{Serialize, SerializeStruct, Serializer};
use sorella_db_databases::clickhouse::{fixed_string::FixedString, DbRow};

use super::normalized_actions::ClickhouseVecNormalizedMintOrBurn;
use crate::{
    classified_mev::JitLiquidity,
    serde_utils::{
        gas_details::ClickhouseVecGasDetails, normalized_actions::ClickhouseDoubleVecNormalizedSwap,
    },
};

impl Serialize for JitLiquidity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut ser_struct = serializer.serialize_struct("JitLiquidity", 30)?;

        // frontrun mint
        ser_struct.serialize_field(
            "frontrun_mint_tx_hash",
            &FixedString::from(format!("{:?}", self.frontrun_mint_tx_hash)),
        )?;

        let frontrun_mints: ClickhouseVecNormalizedMintOrBurn = self.frontrun_mints.clone().into();

        ser_struct.serialize_field("frontrun_mints.trace_idx", &frontrun_mints.trace_index)?;
        ser_struct.serialize_field("frontrun_mints.from", &frontrun_mints.from)?;
        ser_struct.serialize_field("frontrun_mints.to", &frontrun_mints.to)?;
        ser_struct.serialize_field("frontrun_mints.recipient", &frontrun_mints.recipient)?;
        ser_struct.serialize_field("frontrun_mints.tokens", &frontrun_mints.tokens)?;
        ser_struct.serialize_field("frontrun_mints.amounts", &frontrun_mints.amounts)?;

        let frontrun_mint_gas_details = (
            self.frontrun_mint_gas_details.coinbase_transfer,
            self.frontrun_mint_gas_details.priority_fee,
            self.frontrun_mint_gas_details.gas_used,
            self.frontrun_mint_gas_details.effective_gas_price,
        );

        ser_struct.serialize_field("frontrun_mint_gas_details", &(frontrun_mint_gas_details))?;

        // victim swaps
        let victim_swaps: ClickhouseDoubleVecNormalizedSwap =
            (self.victim_swaps_tx_hashes.clone(), self.victim_swaps.clone()).into();
        ser_struct.serialize_field("victim_swaps.tx_hash", &victim_swaps.tx_hash)?;
        ser_struct.serialize_field("victim_swaps.trace_idx", &victim_swaps.trace_index)?;
        ser_struct.serialize_field("victim_swaps.from", &victim_swaps.from)?;
        ser_struct.serialize_field("victim_swaps.recipient", &victim_swaps.recipient)?;
        ser_struct.serialize_field("victim_swaps.pool", &victim_swaps.pool)?;
        ser_struct.serialize_field("victim_swaps.token_in", &victim_swaps.token_in)?;
        ser_struct.serialize_field("victim_swaps.token_out", &victim_swaps.token_out)?;
        ser_struct.serialize_field("victim_swaps.amount_in", &victim_swaps.amount_in)?;
        ser_struct.serialize_field("victim_swaps.amount_out", &victim_swaps.amount_out)?;

        let victim_gas_details: ClickhouseVecGasDetails = (
            self.victim_swaps_gas_details_tx_hashes.clone(),
            self.victim_swaps_gas_details.clone(),
        )
            .into();
        ser_struct.serialize_field("victim_gas_details.tx_hash", &victim_gas_details.tx_hash)?;
        ser_struct.serialize_field(
            "victim_gas_details.coinbase_transfer",
            &victim_gas_details.coinbase_transfer,
        )?;
        ser_struct
            .serialize_field("victim_gas_details.priority_fee", &victim_gas_details.priority_fee)?;
        ser_struct.serialize_field("victim_gas_details.gas_used", &victim_gas_details.gas_used)?;
        ser_struct.serialize_field(
            "victim_gas_details.effective_gas_price",
            &victim_gas_details.effective_gas_price,
        )?;

        // backrun burn
        ser_struct.serialize_field(
            "backrun_burn_tx_hash",
            &FixedString::from(format!("{:?}", self.backrun_burn_tx_hash)),
        )?;

        let backrun_burns: ClickhouseVecNormalizedMintOrBurn = self.backrun_burns.clone().into();

        ser_struct.serialize_field("backrun_burns.trace_idx", &backrun_burns.trace_index)?;
        ser_struct.serialize_field("backrun_burns.from", &backrun_burns.from)?;
        ser_struct.serialize_field("backrun_burns.to", &backrun_burns.to)?;
        ser_struct.serialize_field("backrun_burns.recipient", &backrun_burns.recipient)?;
        ser_struct.serialize_field("backrun_burns.tokens", &backrun_burns.tokens)?;
        ser_struct.serialize_field("backrun_burns.amounts", &backrun_burns.amounts)?;

        let backrun_burn_gas_details = (
            self.backrun_burn_gas_details.coinbase_transfer,
            self.backrun_burn_gas_details.priority_fee,
            self.backrun_burn_gas_details.gas_used,
            self.backrun_burn_gas_details.effective_gas_price,
        );

        ser_struct.serialize_field("backrun_burn_gas_details", &(backrun_burn_gas_details))?;

        ser_struct.end()
    }
}

impl DbRow for JitLiquidity {
    const COLUMN_NAMES: &'static [&'static str] = &[
        "frontrun_mint_tx_hash",
        "frontrun_mints.trace_idx",
        "frontrun_mints.from",
        "frontrun_mints.to",
        "frontrun_mints.recipient",
        "frontrun_mints.tokens",
        "frontrun_mints.amounts",
        "frontrun_mint_gas_details",
        "victim_swaps.tx_hash",
        "victim_swaps.trace_idx",
        "victim_swaps.from",
        "victim_swaps.recipient",
        "victim_swaps.pool",
        "victim_swaps.token_in",
        "victim_swaps.token_out",
        "victim_swaps.amount_in",
        "victim_swaps.amount_out",
        "victim_gas_details.tx_hash",
        "victim_gas_details.coinbase_transfer",
        "victim_gas_details.priority_fee",
        "victim_gas_details.gas_used",
        "victim_gas_details.effective_gas_price",
        "backrun_burn_tx_hash",
        "backrun_burns.trace_idx",
        "backrun_burns.from",
        "backrun_burns.to",
        "backrun_burns.recipient",
        "backrun_burns.tokens",
        "backrun_burns.amounts",
        "backrun_burn_gas_details",
    ];
}
