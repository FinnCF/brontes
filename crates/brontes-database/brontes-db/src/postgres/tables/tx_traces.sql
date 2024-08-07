-- 1. │ block_number │ UInt64                                                                                                                                                                                                                                                     │              │                    │         │                  │                │
-- 2. │ traces       │ 
-- Array(
--     Tuple(
--         UInt64,
--         Array(Tuple(
--             UInt64, 
--             String, 
--             Nullable(String), 
--             UInt64, 
--             Array(UInt64))), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             Array(Tuple(
--                 String, 
--                 String, 
--                 String)), 
--             Array(Tuple(
--                 String, 
--                 String, 
--                 String)))), 
--         Array(Tuple(
--             UInt64, 
--             UInt64, 
--             String, 
--             Array(String), 
--             String)), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             UInt64, 
--             String, 
--             UInt256)), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             String, 
--             UInt64, 
--             String, 
--             String, 
--             UInt256)), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             UInt256, 
--             String)), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             String, 
--             UInt256)), 
--         Array(Tuple(
--             UInt64, 
--             UInt64, 
--             String)), 
--         Array(Tuple(
--             UInt64, 
--             String, 
--             String, 
--             UInt64)), 
-- tx_hash        String, 
-- gas_used        UInt128, 
-- effective_price        UInt128, 
-- tx_index        UInt64, 
-- is_success        Bool)) 
-- 3. │ last_updated │ UInt64                                                                                                                                                                 

CREATE TABLE brontes_api.tx_traces (
    block_number BIGINT,
    trace JSON,
    last_updated TIMESTAMP NOT NULL DEFAULT NOW()
);