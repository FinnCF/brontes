use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use futures::future::join_all;
use itertools::Itertools;
use reth_db::DatabaseError;
use serde::Deserialize;
use sorella_db_databases::{clickhouse::DbRow, Database};
use tracing::info;

use super::{tables::Tables, types::LibmdbxData, Libmdbx};
use crate::{clickhouse::Clickhouse, libmdbx::types::CompressedTable};

const DEFAULT_START_BLOCK: u64 = 15400000;
// change with tracing client
const DEFAULT_END_BLOCK: u64 = 15400000;

pub struct LibmdbxInitializer {
    libmdbx:    Arc<Libmdbx>,
    clickhouse: Arc<Clickhouse>,
    //tracer:     Arc<TracingClient>,
}

impl LibmdbxInitializer {
    pub fn new(
        libmdbx: Arc<Libmdbx>,
        clickhouse: Arc<Clickhouse>,
        //tracer: Arc<TracingClient>,
    ) -> Self {
        Self { libmdbx, clickhouse } //, tracer }
    }

    pub async fn initialize(
        &self,
        tables: &[Tables],
        block_range: Option<(u64, u64)>, // inclusive of start only
    ) -> eyre::Result<()> {
        join_all(
            tables
                .iter()
                .map(|table| table.initialize_table(&self, block_range)),
        )
        .await
        .into_iter()
        .collect::<eyre::Result<_>>()
    }

    pub(crate) async fn initialize_table_from_clickhouse<'db, T, D>(
        &'db self,
        block_range: Option<(u64, u64)>,
    ) -> eyre::Result<()>
    where
        T: CompressedTable,
        T::Value: From<T::DecompressedValue> + Into<T::DecompressedValue>,
        D: LibmdbxData<T> + DbRow + for<'de> Deserialize<'de> + Send + Sync + Debug + 'static,
    {
        self.libmdbx.clear_table::<T>()?;

        let block_range_chunks = if let Some((s, e)) = block_range {
            (s..e).chunks(T::INIT_CHUNK_SIZE.unwrap_or((e - s + 1) as usize))
        } else {
            (DEFAULT_START_BLOCK..DEFAULT_END_BLOCK).chunks(
                T::INIT_CHUNK_SIZE
                    .unwrap_or((DEFAULT_END_BLOCK - DEFAULT_START_BLOCK + 1) as usize),
            )
        };

        let pair_ranges = block_range_chunks
            .into_iter()
            .map(|chk| chk.into_iter().collect_vec())
            .filter_map(
                |chk| if chk.len() != 0 { Some((chk[0], chk[chk.len() - 1])) } else { None },
            )
            .collect_vec();

        let num_chunks = Arc::new(Mutex::new(pair_ranges.len()));

        info!(target: "brontes::init", "{} -- Starting Initialization With {} Chunks", T::NAME, pair_ranges.len());
        join_all(pair_ranges.into_iter().map(|(start, end)| {let num_chunks = num_chunks.clone(); async move {
            let data = self
                .clickhouse
                .inner()
                .query_many::<D>(T::INIT_QUERY.expect("Should only be called on clickhouse tables"), &(start, end))
                .await;

            let num = {
                let mut n = num_chunks.lock().unwrap();
                *n -= 1;
                n.clone() + 1
            };

            match data {
                Ok(d) => self.libmdbx.write_table(&d)?,
                Err(e) => {
                    info!(target: "brontes::init", "{} -- Error Writing Chunk {} -- {:?}", T::NAME, num, e)
                }
            }

            info!(target: "brontes::init", "{} -- Finished Chunk {}", T::NAME, num);

            Ok::<(), DatabaseError>(())
        }}))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{env, path::Path, sync::Arc};

    use brontes_pricing::types::PoolState;
    use brontes_types::classified_mev::{BundleHeader, MevBlock, Sandwich};
    use reth_db::{cursor::DbCursorRO, transaction::DbTx, DatabaseError};
    use reth_tracing_ext::TracingClient;
    use serial_test::serial;

    use super::LibmdbxInitializer;
    use crate::{clickhouse::Clickhouse, libmdbx::*};

    fn init_db() -> eyre::Result<Libmdbx> {
        dotenv::dotenv().ok();
        let brontes_db_path = env::var("BRONTES_DB_PATH").expect("No BRONTES_DB_PATH in .env");
        Libmdbx::init_db(brontes_db_path, None)
    }

    async fn initialize_tables(tables: &[Tables]) -> eyre::Result<Arc<Libmdbx>> {
        let db = Arc::new(init_db()?);
        let clickhouse = Clickhouse::default();

        let db_initializer = LibmdbxInitializer::new(db.clone(), Arc::new(clickhouse));
        db_initializer.initialize(tables, None).await?;

        Ok(db)
    }

    /*
       async fn initialize_tables(tables: &[Tables]) -> eyre::Result<Arc<Libmdbx>> {
           let db = Arc::new(init_db()?);
           let clickhouse = Clickhouse::default();

           let db_path = env::var("DB_PATH")
               .map_err(|_| Box::new(std::env::VarError::NotPresent))
               .unwrap();
           let (manager, tracer) =
               TracingClient::new(Path::new(&db_path), tokio::runtime::Handle::current(), 10);
           tokio::spawn(manager);

           let tracer = Arc::new(tracer);
           let db_initializer = LibmdbxInitializer::new(db.clone(), Arc::new(clickhouse), tracer);
           db_initializer.initialize(tables, None).await?;

           Ok(db)
       }
    */
    async fn test_tokens_decimals_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<TokenDecimals>()?, 0);

        let mut cursor = tx.cursor_read::<TokenDecimals>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }

        Ok(())
    }

    async fn test_address_to_tokens_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<AddressToTokens>()?, 0);

        let mut cursor = tx.cursor_read::<AddressToTokens>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }

    async fn test_address_to_protocols_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<AddressToProtocol>()?, 0);

        let mut cursor = tx.cursor_read::<AddressToProtocol>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }

    async fn test_cex_mapping_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<CexPrice>()?, 0);

        let mut cursor = tx.cursor_read::<CexPrice>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }

    async fn test_metadata_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<Metadata>()?, 0);

        let mut cursor = tx.cursor_read::<Metadata>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }
    /*
    async fn test_pool_state_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = LibmdbxTx::new_ro_tx(&db.0)?;
        assert_ne!(tx.entries::<PoolState>()?, 0);

        let mut cursor = tx.cursor_read::<PoolState>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }


        async fn test_dex_price_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
            let tx = LibmdbxTx::new_ro_tx(&db.0)?;
            assert_ne!(tx.entries::<DexPrice>()?, 0);

            let mut cursor = tx.cursor_dup_read::<DexPrice>()?;

            if !print {
                cursor.first()?.ok_or(DatabaseError::Read(-1))?;
            } else {
                while let Some(vals) = cursor.next()? {
                    println!("{:?}\n", vals);
                }
            }

            println!("\n\n\n\n");

            cursor.first()?;
            let mut dup_walk = cursor.walk_dup(Some(10), None)?;
            if !print {
                let _ = dup_walk.next().ok_or(DatabaseError::Read(-1))?;
            } else {
                while let Some(vals) = dup_walk.next() {
                    println!("{:?}\n", vals);
                }
            }
            /*
            assert!(first_dup.is_some());
            println!("\n\n{:?}", first_dup);

            let next_dup = cursor.next_dup()?;
            assert!(next_dup.is_some());
            println!("\n\n{:?}", next_dup);
            */
            Ok(())
        }
    */
    async fn test_pool_creation_blocks_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
        let tx = db.ro_tx()?;
        assert_ne!(tx.entries::<PoolCreationBlocks>()?, 0);

        let mut cursor = tx.cursor_read::<PoolCreationBlocks>()?;
        if !print {
            cursor.first()?.ok_or(DatabaseError::Read(-1))?;
        } else {
            while let Some(vals) = cursor.next()? {
                println!("{:?}", vals);
            }
        }
        Ok(())
    }

    fn test_classified_mev_inserts(db: &Libmdbx) -> eyre::Result<()> {
        let block = MevBlock { ..Default::default() };
        let classified_mev = BundleHeader::default();
        let specific_mev = Sandwich::default();

        //let _ = db.insert_classified_data(block, vec![(classified_mev,
        // Box::new(specific_mev))]);

        Ok(())
    }

    /* /
        async fn test_tx_traces_table(db: &Libmdbx, print: bool) -> eyre::Result<()> {
            let tx = LibmdbxTx::new_ro_tx(&db.0)?;
            assert_ne!(tx.entries::<TxTracesDB>()?, 0);

            let mut cursor = tx.cursor_read::<TxTracesDB>()?;
            if !print {
                cursor.first()?.ok_or(DatabaseError::Read(-1))?;
            } else {
                while let Some(vals) = cursor.next()? {
                    println!("{:?}", vals);
                }
            }
            Ok(())
        }
    */
    #[tokio::test(flavor = "multi_thread", worker_threads = 5)]
    #[serial]
    async fn test_inserts() {
        let db = init_db().unwrap();

        let q = test_classified_mev_inserts(&db);
        assert!(q.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 20)]
    #[serial]
    async fn test_intialize_tables() {
        let db = initialize_tables(&[
            //Tables::TokenDecimals,
            //Tables::AddressToTokens,
            //Tables::AddressToProtocol,
            //Tables::CexPrice,
            //Tables::Metadata,
            //Tables::PoolState,
            //Tables::DexPrice,
            Tables::PoolCreationBlocks,
            // Tables::TxTraces,
        ])
        .await;
        assert!(db.is_ok());

        let db = db.unwrap();
        //assert!(test_tokens_decimals_table(&db, false).await.is_ok());
        //assert!(test_address_to_tokens_table(&db, false).await.is_ok());
        //assert!(test_address_to_protocols_table(&db, false).await.is_ok());
        //assert!(test_cex_mapping_table(&db, false).await.is_ok());
        //assert!(test_metadata_table(&db, false).await.is_ok());
        //assert!(test_pool_state_table(&db, false).await.is_ok());
        //assert!(test_dex_price_table(&db, false).await.is_ok());
        assert!(test_pool_creation_blocks_table(&db, false).await.is_ok());
        // assert!(test_tx_traces_table(&db, true).await.is_ok());
    }
}
