// Copyright 2023 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Provides E2E Test runner functionality.
use itertools::Itertools;
use rand::{Rng, SeedableRng};
use tokio_postgres::error::Error as PgError;

use crate::validation::is_permissible_error;
use crate::{create_table_statement_to_table, mview_sql_gen, parse_sql, sql_gen, Table};

/// e2e test runner for sqlsmith
pub async fn run(client: &tokio_postgres::Client, testdata: &str, count: usize) {
    let mut rng = rand::rngs::SmallRng::from_entropy();
    let (tables, mviews, setup_sql) = create_tables(&mut rng, testdata, client).await;

    test_sqlsmith(client, &mut rng, tables.clone(), &setup_sql).await;
    tracing::info!("Passed sqlsmith tests");
    test_batch_queries(client, &mut rng, tables.clone(), &setup_sql, count).await;
    tracing::info!("Passed batch queries");
    test_stream_queries(client, &mut rng, tables.clone(), &setup_sql, count).await;
    tracing::info!("Passed stream queries");

    drop_tables(&mviews, testdata, client).await;
}

/// Sanity checks for sqlsmith
async fn test_sqlsmith<R: Rng>(
    client: &tokio_postgres::Client,
    rng: &mut R,
    tables: Vec<Table>,
    setup_sql: &str,
) {
    // Test percentage of skipped queries <=5% of sample size.
    let threshold = 0.20; // permit at most 20% of queries to be skipped.
    let sample_size = 50;

    let skipped_percentage =
        test_batch_queries(client, rng, tables.clone(), setup_sql, sample_size).await;
    if skipped_percentage > threshold {
        panic!(
            "percentage of skipped batch queries = {}, threshold: {}",
            skipped_percentage, threshold
        );
    }

    let skipped_percentage =
        test_stream_queries(client, rng, tables.clone(), setup_sql, sample_size).await;
    if skipped_percentage > threshold {
        panic!(
            "percentage of skipped stream queries = {}, threshold: {}",
            skipped_percentage, threshold
        );
    }
}

/// Test batch queries, returns skipped query statistics
/// Runs in distributed mode, since queries can be complex and cause overflow in local execution
/// mode.
async fn test_batch_queries<R: Rng>(
    client: &tokio_postgres::Client,
    rng: &mut R,
    tables: Vec<Table>,
    setup_sql: &str,
    sample_size: usize,
) -> f64 {
    client
        .query("SET query_mode TO distributed;", &[])
        .await
        .unwrap();
    let mut skipped = 0;
    for _ in 0..sample_size {
        let sql = sql_gen(rng, tables.clone());
        tracing::info!("Executing: {}", sql);
        let response = client.query(sql.as_str(), &[]).await;
        skipped += validate_response(setup_sql, &format!("{};", sql), response);
    }
    skipped as f64 / sample_size as f64
}

/// Test stream queries, returns skipped query statistics
async fn test_stream_queries<R: Rng>(
    client: &tokio_postgres::Client,
    rng: &mut R,
    tables: Vec<Table>,
    setup_sql: &str,
    sample_size: usize,
) -> f64 {
    let mut skipped = 0;
    for _ in 0..sample_size {
        let (sql, table) = mview_sql_gen(rng, tables.clone(), "stream_query");
        tracing::info!("Executing: {}", sql);
        let response = client.execute(&sql, &[]).await;
        skipped += validate_response(setup_sql, &format!("{};", sql), response);
        drop_mview_table(&table, client).await;
    }
    skipped as f64 / sample_size as f64
}

fn get_seed_table_sql(testdata: &str) -> String {
    let seed_files = vec!["tpch.sql", "nexmark.sql", "alltypes.sql"];
    seed_files
        .iter()
        .map(|filename| std::fs::read_to_string(format!("{}/{}", testdata, filename)).unwrap())
        .collect::<String>()
}

async fn create_tables(
    rng: &mut impl Rng,
    testdata: &str,
    client: &tokio_postgres::Client,
) -> (Vec<Table>, Vec<Table>, String) {
    tracing::info!("Preparing tables...");
    let sqls = "SET query_mode TO distributed;
    ---- START
    -- Setup
    CREATE TABLE supplier (s_suppkey INT, s_name CHARACTER VARYING, s_address CHARACTER VARYING, s_nationkey INT, s_phone CHARACTER VARYING, s_acctbal NUMERIC, s_comment CHARACTER VARYING, PRIMARY KEY (s_suppkey));CREATE TABLE part (p_partkey INT, p_name CHARACTER VARYING, p_mfgr CHARACTER VARYING, p_brand CHARACTER VARYING, p_type CHARACTER VARYING, p_size INT, p_container CHARACTER VARYING, p_retailprice NUMERIC, p_comment CHARACTER VARYING, PRIMARY KEY (p_partkey));CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_availqty INT, ps_supplycost NUMERIC, ps_comment CHARACTER VARYING, PRIMARY KEY (ps_partkey, ps_suppkey));CREATE TABLE customer (c_custkey INT, c_name CHARACTER VARYING, c_address CHARACTER VARYING, c_nationkey INT, c_phone CHARACTER VARYING, c_acctbal NUMERIC, c_mktsegment CHARACTER VARYING, c_comment CHARACTER VARYING, PRIMARY KEY (c_custkey));CREATE TABLE orders (o_orderkey BIGINT, o_custkey INT, o_orderstatus CHARACTER VARYING, o_totalprice NUMERIC, o_orderdate DATE, o_orderpriority CHARACTER VARYING, o_clerk CHARACTER VARYING, o_shippriority INT, o_comment CHARACTER VARYING, PRIMARY KEY (o_orderkey));CREATE TABLE lineitem (l_orderkey BIGINT, l_partkey INT, l_suppkey INT, l_linenumber INT, l_quantity NUMERIC, l_extendedprice NUMERIC, l_discount NUMERIC, l_tax NUMERIC, l_returnflag CHARACTER VARYING, l_linestatus CHARACTER VARYING, l_shipdate DATE, l_commitdate DATE, l_receiptdate DATE, l_shipinstruct CHARACTER VARYING, l_shipmode CHARACTER VARYING, l_comment CHARACTER VARYING, PRIMARY KEY (l_orderkey, l_linenumber));CREATE TABLE nation (n_nationkey INT, n_name CHARACTER VARYING, n_regionkey INT, n_comment CHARACTER VARYING, PRIMARY KEY (n_nationkey));CREATE TABLE region (r_regionkey INT, r_name CHARACTER VARYING, r_comment CHARACTER VARYING, PRIMARY KEY (r_regionkey));CREATE TABLE person (id BIGINT, name CHARACTER VARYING, email_address CHARACTER VARYING, credit_card CHARACTER VARYING, city CHARACTER VARYING, state CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING, PRIMARY KEY (id));CREATE TABLE auction (id BIGINT, item_name CHARACTER VARYING, description CHARACTER VARYING, initial_bid BIGINT, reserve BIGINT, date_time TIMESTAMP, expires TIMESTAMP, seller BIGINT, category BIGINT, extra CHARACTER VARYING, PRIMARY KEY (id));CREATE TABLE bid (auction BIGINT, bidder BIGINT, price BIGINT, channel CHARACTER VARYING, url CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING);CREATE TABLE alltypes1 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);CREATE TABLE alltypes2 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);CREATE MATERIALIZED VIEW m0 AS SELECT false AS col_0, SMALLINT '32767' AS col_1, INT '2147483647' AS col_2 FROM (WITH with_0 AS (SELECT INT '2147483647' AS col_0 FROM nation AS t_1 WHERE true GROUP BY t_1.n_comment, t_1.n_nationkey HAVING approx_count_distinct(TIMESTAMP '2022-07-03 15:05:47') > 1) SELECT NULL AS col_0, INT '1' AS col_1, TIMESTAMP '2022-07-03 15:05:47' AS col_2, TIME '15:05:47' AS col_3 FROM with_0 WHERE false) AS sq_2 WHERE INTERVAL '873864' IS NOT NULL GROUP BY sq_2.col_3, sq_2.col_2, sq_2.col_0, sq_2.col_1 HAVING true;CREATE MATERIALIZED VIEW m1 AS SELECT sq_1.col_0 AS col_0, INTERVAL '1' AS col_1, 2147483647 AS col_2, 0 AS col_3 FROM (SELECT DATE '2022-07-09' AS col_0, REAL '0' AS col_1 FROM nation AS t_0 GROUP BY t_0.n_regionkey, t_0.n_nationkey, t_0.n_name) AS sq_1 GROUP BY sq_1.col_0, sq_1.col_1 HAVING INT '2147483647' <> (SMALLINT '1' - SMALLINT '18842');CREATE MATERIALIZED VIEW m2 AS SELECT false AS col_0 FROM lineitem AS t_0 WHERE t_0.l_orderkey <= FLOAT '0' GROUP BY t_0.l_receiptdate, t_0.l_tax, t_0.l_discount, t_0.l_shipdate, t_0.l_linenumber, t_0.l_commitdate, t_0.l_quantity, t_0.l_comment HAVING max(false);CREATE MATERIALIZED VIEW m3 AS SELECT FLOAT '0' AS col_0, SMALLINT '0' AS col_1 FROM bid AS t_0 GROUP BY t_0.channel, t_0.url, t_0.extra, t_0.bidder, t_0.date_time;CREATE MATERIALIZED VIEW m4 AS SELECT TIMESTAMP '2022-07-10 15:05:48' AS col_0 FROM m1 AS t_0 WHERE true GROUP BY t_0.col_3, t_0.col_0, t_0.col_2, t_0.col_1 HAVING true;CREATE MATERIALIZED VIEW m5 AS SELECT 'SZIGb9XkJE' AS col_0, TIMESTAMP '2022-07-09 15:05:50' AS col_1, 'mOIHbZIHOW' AS col_2, t_0.o_orderstatus AS col_3 FROM orders AS t_0 WHERE true GROUP BY t_0.o_orderstatus, t_0.o_totalprice, t_0.o_clerk, t_0.o_orderkey, t_0.o_orderpriority, t_0.o_comment, t_0.o_custkey, t_0.o_shippriority;CREATE MATERIALIZED VIEW m6 AS SELECT TRIM(TRAILING OVERLAY('symAVjRJks' PLACING 'xr5kfX8lMw' FROM INT '183765822') FROM 'Zfh2uDYBGJ') AS col_0 FROM auction AS t_0 JOIN supplier AS t_1 ON t_0.description = t_1.s_comment WHERE false GROUP BY t_0.expires, t_0.reserve, t_1.s_comment, t_0.seller HAVING CAST(~ (INT '1' # INT '1633980496') AS BOOLEAN);CREATE MATERIALIZED VIEW m7 AS SELECT (SMALLINT '32767' | CASE WHEN TIMESTAMP '2022-07-10 14:05:51' = TIMESTAMP '2022-07-10 15:04:51' THEN INT '0' WHEN t_0.col_0 THEN INT '356056457' WHEN t_0.col_0 THEN INT '0' WHEN true IS TRUE THEN INT '2147483647' WHEN true THEN INT '1388255714' ELSE INT '2147483647' - INT '1' END) + DATE '2022-07-10' AS col_0, REAL '747261976.941812' AS col_1 FROM m0 AS t_0 WHERE CAST(t_0.col_2 AS BOOLEAN) GROUP BY t_0.col_0 HAVING true;CREATE MATERIALIZED VIEW m8 AS SELECT 'LmV9gIYUYx' AS col_0 FROM alltypes2 AS t_0 WHERE INT '2103263379' = t_0.c4 GROUP BY t_0.c5, t_0.c6, t_0.c16, t_0.c13, t_0.c2, t_0.c4, t_0.c15, t_0.c1, t_0.c10, t_0.c11, t_0.c8, t_0.c3, t_0.c14 HAVING (t_0.c2 - t_0.c2) > SMALLINT '19835';CREATE MATERIALIZED VIEW m9 AS SELECT FLOAT '0' - FLOAT '106639522.95117745' AS col_0, FLOAT '2147483647' AS col_1 FROM orders AS t_0 WHERE true GROUP BY t_0.o_orderstatus, t_0.o_custkey, t_0.o_orderkey, t_0.o_clerk, t_0.o_shippriority, t_0.o_orderpriority, t_0.o_comment, t_0.o_orderdate HAVING true;
    -- Query
    SELECT tumble_3.c8 AS col_0, 'Em3ZiOpFOj' AS col_1 FROM m3 AS t_0, supplier AS t_1, auction AS t_2, tumble(alltypes2, alltypes2.c11, INTERVAL '1') AS tumble_3, supplier AS t_4, m0 AS t_5, nation AS t_6, part AS t_7, alltypes1 AS t_8, (WITH with_9 AS (SELECT t_17.s_suppkey AS col_0, CAST(false AS INT) AS col_1 FROM part AS t_10 JOIN m8 AS t_11 ON t_10.p_name = t_11.col_0, lineitem AS t_12, auction AS t_13 JOIN partsupp AS t_14 ON t_13.extra = t_14.ps_comment, m9 AS t_15, m2 AS t_16, supplier AS t_17, supplier AS t_20, m1 AS t_21, person AS t_22, m4 AS t_23, m4 AS t_24, m1 AS t_25 WHERE t_16.col_0 GROUP BY t_14.ps_suppkey, t_22.email_address, t_12.l_commitdate, t_12.l_comment, t_12.l_linenumber, t_25.col_0, t_12.l_tax, t_13.description, t_12.l_quantity, t_20.s_suppkey, t_10.p_partkey, t_20.s_comment, t_17.s_nationkey, t_10.p_brand, t_13.expires, t_13.reserve, t_21.col_2, t_17.s_suppkey, t_10.p_retailprice, t_16.col_0, t_12.l_discount, t_14.ps_comment, t_21.col_1, t_22.extra, t_21.col_0, t_13.date_time, t_12.l_linestatus, t_21.col_3, t_20.s_name, t_17.s_acctbal, t_22.name, t_22.state, t_10.p_comment, t_20.s_acctbal, t_13.id, t_10.p_mfgr, t_25.col_2, t_12.l_receiptdate, t_17.s_phone, t_10.p_container, t_13.seller, t_14.ps_availqty, t_10.p_type, t_12.l_suppkey, t_13.initial_bid, t_17.s_address, t_25.col_1, t_22.date_time, t_12.l_partkey, t_13.extra, t_23.col_0, t_12.l_returnflag) SELECT 2147483647 AS col_0, TIME '15:06:08' AS col_1 FROM with_9, m0 AS t_26, auction AS t_27, (SELECT 0 AS col_0, FLOAT '1074969412.9864857' AS col_1, EXISTS (SELECT FLOAT '655300269.2749904' AS col_0, DATE '2022-07-10' AS col_1, (SMALLINT '32767' % ((SMALLINT '32767' << INT '1337681439') << SMALLINT '0')) * SMALLINT '10658' AS col_2, (t_101.l_orderkey % SMALLINT '0') * INT '1' AS col_3 FROM m9 AS t_99, supplier AS t_100, lineitem AS t_101, part AS t_102, part AS t_103 JOIN person AS t_104 ON t_103.p_brand = t_104.city, tumble(auction, auction.date_time, INTERVAL '1') AS tumble_105, m7 AS t_106, (SELECT t_111.c8 AS col_0 FROM m1 AS t_107, m9 AS t_108, customer AS t_109, tumble(bid, bid.date_time, INTERVAL '3600') AS tumble_110, alltypes2 AS t_111, m5 AS t_112, auction AS t_113, lineitem AS t_114 WHERE t_111.c1 GROUP BY t_111.c15, tumble_110.extra, t_111.c8) AS sq_115, orders AS t_116, tumble(alltypes2, alltypes2.c11, INTERVAL '86400') AS tumble_117, orders AS t_118, nation AS t_119 GROUP BY t_103.p_brand, tumble_117.c15, t_104.date_time, t_116.o_orderdate, t_116.o_comment, t_103.p_comment, t_116.o_custkey, t_102.p_brand, t_100.s_phone, t_99.col_0, t_116.o_orderkey, t_102.p_retailprice, tumble_117.c14, t_104.email_address, t_100.s_comment, t_101.l_orderkey, tumble_105.initial_bid, tumble_105.id, t_102.p_mfgr, t_119.n_nationkey, t_101.l_shipdate, t_116.o_orderstatus, tumble_117.c8, t_101.l_quantity, tumble_117.c9, t_100.s_suppkey, t_104.city, t_116.o_clerk HAVING true) AS col_2 FROM m2 AS t_28, lineitem AS t_29, alltypes1 AS t_32, (SELECT 9223372036854775807 AS col_0, t_37.c6 AS col_1, SMALLINT '32767' AS col_2 FROM m7 AS t_33, m8 AS t_34, region AS t_35, hop(person, person.date_time, INTERVAL '1', INTERVAL '641818') AS hop_36, alltypes1 AS t_37, m2 AS t_38, nation AS t_39 JOIN person AS t_40 ON t_39.n_comment = t_40.email_address, m0 AS t_41, m4 AS t_42 GROUP BY t_41.col_0, t_38.col_0, t_35.r_name, t_40.date_time, t_37.c2, t_40.id, t_39.n_comment, t_37.c10, t_41.col_1, t_34.col_0, t_37.c15, t_37.c13, t_37.c7, hop_36.extra, hop_36.email_address, hop_36.id, t_39.n_nationkey, hop_36.city, t_37.c3, t_40.state, t_37.c4, t_37.c14, t_33.col_1, t_37.c8, t_40.city, t_37.c1, t_35.r_regionkey, t_40.credit_card, t_37.c9, t_37.c6, t_40.name, t_41.col_2, hop_36.date_time, t_37.c5, hop_36.credit_card, t_35.r_comment, t_33.col_0, t_40.email_address, hop_36.name, hop_36.state, t_39.n_regionkey, t_39.n_name, t_42.col_0, t_40.extra, t_37.c16 HAVING CAST(false AS INT) < FLOAT '1') AS sq_43, m7 AS t_44, alltypes1 AS t_45, part AS t_46, person AS t_47, m0 AS t_48, alltypes2 AS t_49, nation AS t_50 JOIN customer AS t_51 ON t_50.n_comment = t_51.c_address, customer AS t_52, (WITH with_53 AS (SELECT FLOAT '721490665.9341156' AS col_0, INTERVAL '3600' AS col_1 FROM m8 AS t_54, m1 AS t_55, m8 AS t_56, m5 AS t_57 JOIN alltypes1 AS t_58 ON t_57.col_3 = t_58.c9, auction AS t_59, auction AS t_60, tumble(m4, m4.col_0, INTERVAL '60') AS tumble_61, m9 AS t_62, tumble(person, person.date_time, INTERVAL '1') AS tumble_63, orders AS t_64, tumble(alltypes2, alltypes2.c11, INTERVAL '3600') AS tumble_65, alltypes1 AS t_66 JOIN alltypes1 AS t_67 ON t_66.c6 = t_67.c6 GROUP BY t_58.c10, t_55.col_2, tumble_61.col_0, tumble_65.c1, t_55.col_3, t_60.category, t_60.seller, tumble_63.name, t_67.c6, t_59.category, t_59.seller, tumble_65.c13, t_66.c15) SELECT (t_75.c6 / ((FLOAT '0' / sum(t_70.c5)) * REAL '1')) * t_75.c5 AS col_0 FROM with_53, m0 AS t_68, customer AS t_69, alltypes2 AS t_70, m8 AS t_71, m8 AS t_72, m2 AS t_73, person AS t_74, alltypes1 AS t_75, auction AS t_76, part AS t_77, auction AS t_78, orders AS t_79 WHERE t_75.c1 GROUP BY t_78.extra, t_69.c_address, t_75.c5, t_78.initial_bid, t_75.c15, t_76.category, t_79.o_orderstatus, t_70.c8, t_77.p_container, t_77.p_size, t_76.date_time, t_74.name, t_78.seller, t_75.c2, t_70.c3, t_77.p_name, t_75.c7, t_75.c6, t_76.expires, t_70.c9, t_75.c13, t_70.c1, t_70.c7, t_79.o_orderdate, t_79.o_orderpriority, t_76.item_name, t_69.c_mktsegment, t_74.credit_card, t_79.o_custkey, t_69.c_phone, t_74.state, t_77.p_type, t_76.extra, t_76.id, t_75.c9, t_79.o_orderkey, t_75.c4, t_70.c13, t_75.c14, t_69.c_name, t_75.c1, t_70.c11, t_74.city, t_79.o_comment, t_76.seller, t_74.email_address, t_69.c_acctbal, t_76.description, t_69.c_custkey, t_77.p_partkey, t_72.col_0, t_76.initial_bid, t_79.o_clerk HAVING t_70.c1 LIMIT 27) AS sq_80, m0 AS t_81, (SELECT 9223372036854775807 AS col_0, INTERVAL '851017' AS col_1, SMALLINT '16932' AS col_2, FLOAT '1' AS col_3 FROM m4 AS t_82, m0 AS t_83, m6 AS t_84, person AS t_85, m9 AS t_86, m7 AS t_87, region AS t_88, lineitem AS t_89 JOIN partsupp AS t_90 ON t_89.l_partkey = t_90.ps_suppkey, m5 AS t_91, bid AS t_92, person AS t_93, m1 AS t_94, alltypes1 AS t_95, m6 AS t_96 WHERE t_89.l_suppkey < 1 GROUP BY t_95.c2, t_89.l_linestatus, t_82.col_0, t_85.name, t_83.col_1, t_93.extra, t_85.date_time, t_94.col_1, t_89.l_linenumber, t_91.col_1, t_95.c14, t_93.name, t_95.c7, t_95.c9, t_83.col_0, t_85.extra, t_95.c1, t_93.email_address, t_95.c3, t_89.l_comment, t_92.channel, t_93.state, t_89.l_tax, t_92.date_time, t_92.price, t_83.col_2, t_85.city, t_89.l_returnflag, t_95.c10, t_89.l_shipinstruct, t_90.ps_availqty, t_86.col_1, t_89.l_discount, t_92.bidder, t_94.col_2, t_91.col_0, t_87.col_1, t_90.ps_partkey, t_87.col_0, t_89.l_quantity, t_90.ps_supplycost, t_90.ps_suppkey, t_85.state, t_96.col_0, t_85.credit_card, t_94.col_3, t_86.col_0, t_91.col_3, t_85.email_address, t_88.r_regionkey, t_95.c16, t_89.l_shipmode, t_93.date_time, t_89.l_receiptdate, t_88.r_comment, t_92.auction, t_93.city, t_95.c15, t_90.ps_comment, t_95.c13, t_89.l_commitdate, t_95.c4, t_91.col_2, t_95.c8, t_93.credit_card, t_89.l_shipdate, t_92.extra, t_95.c6, t_93.id, t_88.r_name, t_85.id, t_89.l_suppkey, t_92.url, t_84.col_0, t_95.c5, t_89.l_orderkey, t_94.col_0, t_89.l_partkey, t_95.c11 HAVING t_83.col_0) AS sq_97, m2 AS t_98 WHERE t_45.c1 GROUP BY sq_80.col_0, t_32.c9, t_29.l_comment, t_46.p_partkey, t_50.n_nationkey, sq_97.col_3, t_51.c_address, t_44.col_1, t_49.c11, t_45.c11, t_49.c10, t_29.l_receiptdate, t_48.col_0, t_32.c11, t_29.l_discount, t_32.c10, t_50.n_regionkey, t_51.c_phone, t_52.c_name, t_46.p_comment, t_45.c9, t_45.c7, t_32.c7, t_49.c16, t_50.n_name, t_47.name, t_48.col_1, t_51.c_comment, t_49.c15, sq_43.col_0, t_32.c2, t_52.c_acctbal, t_29.l_shipdate, t_47.email_address, t_81.col_2, t_45.c14, t_49.c4, t_45.c10, t_46.p_type, t_81.col_1, t_52.c_custkey, t_46.p_container, t_47.extra, t_48.col_2, t_32.c5, t_49.c1, t_49.c14, t_51.c_nationkey, t_29.l_orderkey, t_47.credit_card, t_45.c8, t_47.id, t_32.c8) AS sq_120, m3 AS t_121, part AS t_122, supplier AS t_123 JOIN orders AS t_124 ON t_123.s_phone = t_124.o_orderpriority, orders AS t_125, m5 AS t_126, m0 AS t_127, m3 AS t_128, customer AS t_129, part AS t_130 GROUP BY t_124.o_totalprice, t_130.p_type, t_124.o_custkey, t_124.o_comment, t_122.p_mfgr, t_129.c_comment, t_27.expires, t_123.s_acctbal, t_122.p_container, t_127.col_1, t_27.id, t_26.col_0, t_130.p_size, t_27.date_time, t_27.category, t_27.reserve, t_123.s_suppkey, t_27.seller, t_26.col_1, t_130.p_retailprice, t_128.col_1, t_122.p_partkey, t_27.item_name, t_130.p_container, t_125.o_totalprice, t_122.p_size, t_122.p_type, t_26.col_2, t_123.s_name, t_124.o_clerk, t_121.col_0, t_124.o_orderkey, t_129.c_nationkey, t_124.o_orderdate, t_126.col_1, sq_120.col_2, t_126.col_0, t_122.p_brand, t_130.p_brand, t_130.p_mfgr, t_127.col_0, t_27.description, sq_120.col_1, t_130.p_name, t_123.s_comment, t_129.c_acctbal, t_126.col_3, sq_120.col_0, t_125.o_comment, t_128.col_0, t_130.p_comment, t_129.c_name, t_125.o_orderkey, t_126.col_2, t_124.o_orderpriority, t_121.col_1, t_27.extra, t_125.o_clerk, t_122.p_name, t_122.p_retailprice, t_124.o_orderstatus, t_129.c_custkey, t_125.o_orderdate, t_27.initial_bid, t_125.o_orderpriority, t_122.p_comment, t_127.col_2, t_129.c_phone, t_125.o_shippriority, t_125.o_custkey, t_129.c_address, t_123.s_phone, t_125.o_orderstatus ORDER BY t_124.o_orderpriority DESC, t_27.seller DESC, t_129.c_address DESC) AS sq_131 WHERE t_5.col_0 GROUP BY t_2.id, t_0.col_0, tumble_3.c3, t_5.col_2, t_2.expires, t_4.s_address, tumble_3.c8, t_7.p_mfgr, t_5.col_1, t_8.c5, tumble_3.c16, t_1.s_name, t_8.c9, t_6.n_name, t_1.s_acctbal, t_4.s_nationkey, t_8.c6, t_7.p_brand, t_1.s_suppkey, tumble_3.c2, t_1.s_nationkey, tumble_3.c13 HAVING TIMESTAMP '2022-07-10 14:10:35' < (INT '1403662392' + DATE '2022-07-10');";
    let statements = parse_sql(&sqls);
    for stmt in statements {
        let sqlstr = stmt.to_string();
        let response = client.execute(&sqlstr, &[]).await;
        validate_response("", "", response);
    }

    let mut setup_sql = String::with_capacity(1000);
    let sql = get_seed_table_sql(testdata);
    let statements = parse_sql(&sql);
    let mut tables = statements
        .iter()
        .map(create_table_statement_to_table)
        .collect_vec();

    for stmt in &statements {
        let create_sql = stmt.to_string();
        setup_sql.push_str(&format!("{};", &create_sql));
        client.execute(&create_sql, &[]).await.unwrap();
    }

    let mut mviews = vec![];
    // Generate some mviews
    for i in 0..10 {
        let (create_sql, table) = mview_sql_gen(rng, tables.clone(), &format!("m{}", i));
        setup_sql.push_str(&format!("{};", &create_sql));
        tracing::info!("Executing MView Setup: {}", &create_sql);
        let response = client.execute(&create_sql, &[]).await;
        let skip_count = validate_response(&setup_sql, &create_sql, response);
        if skip_count == 0 {
            tables.push(table.clone());
            mviews.push(table);
        }
    }
    (tables, mviews, setup_sql)
}

/// Drops mview tables.
async fn drop_mview_table(mview: &Table, client: &tokio_postgres::Client) {
    client
        .execute(
            &format!("DROP MATERIALIZED VIEW IF EXISTS {}", mview.name),
            &[],
        )
        .await
        .unwrap();
}

/// Drops mview tables and seed tables
async fn drop_tables(mviews: &[Table], testdata: &str, client: &tokio_postgres::Client) {
    tracing::info!("Cleaning tables...");

    for mview in mviews.iter().rev() {
        drop_mview_table(mview, client).await;
    }

    let seed_files = vec!["drop_tpch.sql", "drop_nexmark.sql", "drop_alltypes.sql"];
    let sql = seed_files
        .iter()
        .map(|filename| std::fs::read_to_string(format!("{}/{}", testdata, filename)).unwrap())
        .collect::<String>();

    for stmt in sql.lines() {
        client.execute(stmt, &[]).await.unwrap();
    }
}

/// Validate client responses, returning a count of skipped queries.
fn validate_response<_Row>(setup_sql: &str, query: &str, response: Result<_Row, PgError>) -> i64 {
    match response {
        Ok(_) => 0,
        Err(e) => {
            // Permit runtime errors conservatively.
            if let Some(e) = e.as_db_error()
                && is_permissible_error(&e.to_string())
            {
                return 1;
            }
            panic!(
                "
Query failed:
---- START
-- Setup
{}
-- Query
{}
---- END

Reason:
{}
",
                setup_sql, query, e
            );
        }
    }
}
