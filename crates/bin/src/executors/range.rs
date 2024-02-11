use std::{
    pin::Pin,
    task::{Context, Poll},
};

use brontes_core::decoding::TracingProvider;
use brontes_database::libmdbx::{LibmdbxReader, LibmdbxWriter};
use brontes_inspect::Inspector;
use brontes_types::{
    db::metadata::Metadata, mev::Bundle, normalized_actions::Actions, tree::BlockTree,
};
use futures::{pin_mut, stream::FuturesUnordered, Future, StreamExt};
use reth_tasks::shutdown::GracefulShutdown;
use tracing::info;

use super::shared::{inserts::process_results, state_collector::StateCollector};
pub struct RangeExecutorWithPricing<T: TracingProvider, DB: LibmdbxWriter + LibmdbxReader> {
    collector:      StateCollector<T, DB>,
    insert_futures: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,

    current_block: u64,
    end_block:     u64,
    libmdbx:       &'static DB,
    inspectors:    &'static [&'static dyn Inspector<Result = Vec<Bundle>>],
}

impl<T: TracingProvider, DB: LibmdbxReader + LibmdbxWriter> RangeExecutorWithPricing<T, DB> {
    pub fn new(
        start_block: u64,
        end_block: u64,
        state_collector: StateCollector<T, DB>,
        libmdbx: &'static DB,
        inspectors: &'static [&'static dyn Inspector<Result = Vec<Bundle>>],
    ) -> Self {
        Self {
            collector: state_collector,
            insert_futures: FuturesUnordered::default(),
            current_block: start_block,
            end_block,
            libmdbx,
            inspectors,
        }
    }

    pub async fn run_until_graceful_shutdown(self, shutdown: GracefulShutdown) {
        let data_batching = self;
        pin_mut!(data_batching, shutdown);

        let mut graceful_guard = None;
        tokio::select! {
            _ = &mut data_batching => {
            },
            guard = shutdown => {
                graceful_guard = Some(guard);
            },
        }

        drop(graceful_guard);
    }

    fn on_price_finish(&mut self, tree: BlockTree<Actions>, meta: Metadata) {
        info!(target:"brontes","Completed DEX pricing");
        self.insert_futures.push(Box::pin(process_results(
            self.libmdbx,
            self.inspectors,
            tree.into(),
            meta.into(),
        )));
    }
}

impl<T: TracingProvider, DB: LibmdbxReader + LibmdbxWriter> Future
    for RangeExecutorWithPricing<T, DB>
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut work = 256;
        loop {
            if !self.collector.is_collecting_state()
                && self.collector.should_process_next_block()
                && self.current_block != self.end_block
            {
                let block = self.current_block;
                self.collector.fetch_state_for(block);
                self.current_block += 1;
            }

            if let Poll::Ready(result) = self.collector.poll_next_unpin(cx) {
                match result {
                    Some((tree, meta)) => {
                        self.on_price_finish(tree, meta);
                    }
                    None if self.insert_futures.is_empty() => return Poll::Ready(()),
                    _ => {}
                }
            }

            // poll insertion
            while let Poll::Ready(Some(_)) = self.insert_futures.poll_next_unpin(cx) {}

            // mark complete if we are done with the range
            if self.current_block == self.end_block
                && self.insert_futures.is_empty()
                && !self.collector.is_collecting_state()
            {
                self.collector.range_finished();
            }

            work -= 1;
            if work == 0 {
                cx.waker().wake_by_ref();
                return Poll::Pending
            }
        }
    }
}
