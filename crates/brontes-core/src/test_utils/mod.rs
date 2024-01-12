use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use alloy_primitives::Log;
use brontes_database_libmdbx::Libmdbx;
use brontes_metrics::PoirotMetricEvents;
use brontes_types::structured_trace::{TransactionTraceWithLogs, TxTrace};
use log::Level;
use reth_primitives::B256;
use reth_rpc_types::{
    trace::parity::{TraceResults, TransactionTrace},
    TransactionReceipt,
};
use reth_tracing_ext::TracingClient;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    runtime::Handle,
    sync::mpsc::{unbounded_channel, UnboundedSender},
};
use tracing_subscriber::filter::Directive;

use crate::decoding::{parser::TraceParser, TracingProvider};

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct TestTransactionTraceWithLogs {
    pub trace: TransactionTrace,
    pub logs:  Vec<Log>,
}

impl From<TransactionTraceWithLogs> for TestTransactionTraceWithLogs {
    fn from(value: TransactionTraceWithLogs) -> Self {
        Self { trace: value.trace, logs: value.logs }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TestTxTrace {
    pub trace:           Vec<TestTransactionTraceWithLogs>,
    pub tx_hash:         B256,
    pub gas_used:        u128,
    pub effective_price: u128,
    pub tx_index:        u64,
}

impl From<TxTrace> for TestTxTrace {
    fn from(value: TxTrace) -> Self {
        Self {
            trace:           value.trace.into_iter().map(|v| v.into()).collect(),
            tx_hash:         value.tx_hash,
            gas_used:        value.gas_used,
            effective_price: value.effective_price,
            tx_index:        value.tx_index,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestTraceResults {
    pub jsonrpc: String,
    pub result:  TraceResults,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TestTransactionReceipt {
    pub jsonrpc: String,
    pub result:  TransactionReceipt,
}

pub async fn get_full_tx_trace(tx_hash: B256) -> TraceResults {
    let url = "https://reth.sorella-beechit.com:8489";
    let headers = reqwest::header::HeaderMap::from_iter(
        vec![(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap())].into_iter(),
    );

    let payload = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "trace_replayTransaction",
        "params": [&format!("{:#x}", &tx_hash), ["trace", "vmTrace"]]
    });

    let client = reqwest::Client::new();
    let response: TestTraceResults = client
        .post(url)
        .headers(headers)
        .json(&payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    response.result
}

pub async fn get_tx_reciept(tx_hash: B256) -> TransactionReceipt {
    let url = "https://reth.sorella-beechit.com:8489";
    let headers = reqwest::header::HeaderMap::from_iter(
        vec![(reqwest::header::CONTENT_TYPE, "application/json".parse().unwrap())].into_iter(),
    );

    let payload = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "eth_getTransactionReceipt",
        "params": [&format!("{:#x}", &tx_hash)]
    });

    let client = reqwest::Client::new();
    let response: TestTransactionReceipt = client
        .post(url)
        .headers(headers)
        .json(&payload)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    response.result
}

// if we want more tracing/logging/metrics layers, build and push to this vec
// the stdout one (logging) is the only 1 we need
// peep the Database repo -> bin/sorella-db/src/cli.rs line 34 for example
pub fn init_tracing() {
    // all lower level logging directives include higher level ones (Trace includes
    // all, Debug includes all but Trace, ...)
    let verbosity_level = Level::Info; // Error >= Warn >= Info >= Debug >= Trace
    let directive: Directive = format!("{verbosity_level}").parse().unwrap();
    let layers = vec![brontes_tracing::stdout(directive)];

    /*
        make sure the first field of the macro is: 'target: "brontes"',
        otherwise you will get logs from other crates (it's OD annoying trust).

        if you really want tracing from other external crates:
            replace -> let directive: Directive = format!("brontes={verbosity_level}").parse().unwrap();
            with -> let directive: Directive = format!("{verbosity_level}").parse().unwrap();

        to use the logging in a test:
        error!(target: "brontes", ...)
        warn!(target: "brontes", ...)
        info!(target: "brontes", ...)
        debug!(target: "brontes", ...)
        trace!(target: "brontes", ...)
    */

    brontes_tracing::init(layers);
}

pub fn init_trace_parser<'a>(
    handle: Handle,
    metrics_tx: UnboundedSender<PoirotMetricEvents>,
    libmdbx: &'a Libmdbx,
    max_tasks: u32,
) -> TraceParser<'a, Box<dyn TracingProvider>> {
    let db_path = env::var("DB_PATH").expect("No DB_PATH in .env");

    #[cfg(feature = "local")]
    let tracer = {
        let db_endpoint = env::var("RETH_ENDPOINT").expect("No db Endpoint in .env");
        let db_port = env::var("RETH_PORT").expect("No DB port.env");
        let url = format!("{db_endpoint}:{db_port}");
        Box::new(alloy_providers::provider::Provider::new(&url).unwrap())
            as Box<dyn TracingProvider>
    };

    #[cfg(not(feature = "local"))]
    let tracer = {
        let (t_handle, client) =
            TracingClient::new(Path::new(&db_path), handle.clone(), max_tasks as u64);
        handle.spawn(t_handle);

        Box::new(client) as Box<dyn TracingProvider>
    };

    let call = Box::new(|_: &_, _: &_| true);

    TraceParser::new(libmdbx, call, Arc::new(tracer), Arc::new(metrics_tx))
}

pub async fn store_traces_for_block(block_number: u64) {
    let brontes_db_endpoint = env::var("BRONTES_DB_PATH").expect("No BRONTES_DB_PATH in .env");
    let libmdbx = Libmdbx::init_db(brontes_db_endpoint, None).unwrap();

    let (a, b) = unbounded_channel();
    let tracer = init_trace_parser(tokio::runtime::Handle::current(), a, &libmdbx, 10);

    let (block_trace, header) = tracer.execute_block(block_number).await.unwrap();

    let file = PathBuf::from(format!(
        "./crates/brontes-core/src/test_utils/liquidation_traces/{}.json",
        block_number
    ));

    let stringified = serde_json::to_string(&(block_trace, header)).unwrap();
    std::fs::write(&file, stringified).unwrap();
    drop(b)
}
