use std::collections::{HashMap, HashSet};

use brontes_core::decoding::{parser::TraceParser, TracingProvider};
use brontes_database::{database::Database, Metadata};
use brontes_types::{normalized_actions::Actions, structured_trace::TxTrace, tree::TimeTree};
use hex_literal::hex;
use reth_primitives::{alloy_primitives::FixedBytes, Header, H256};

use crate::Classifier;

pub fn helper_build_tree(
    classifier: &Classifier,
    traces: Vec<TxTrace>,
    header: Header,
    metadata: &Metadata,
) -> TimeTree<Actions> {
    classifier.build_tree(traces, header, metadata)
}

pub async fn build_raw_test_tree<T: TracingProvider>(
    tracer: &TraceParser<'_, T>,
    db: &Database,
    block_number: u64,
) -> TimeTree<Actions> {
    let (traces, header, metadata) = get_traces_with_meta(tracer, db, block_number).await;
    let classifier = Classifier::new();
    classifier.build_tree(traces, header, &metadata)
}

pub async fn get_traces_with_meta<T: TracingProvider>(
    tracer: &TraceParser<'_, T>,
    db: &Database,
    block_number: u64,
) -> (Vec<TxTrace>, Header, Metadata) {
    let (traces, header) = tracer.execute_block(block_number).await.unwrap();
    let metadata = db.get_metadata(block_number).await;
    (traces, header, metadata)
}
