/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Common construction utilities for SQL database managers.
//!
//! Mononoke data stores that are backed by SQL databases are managed by a database manager, like
//! `SqlChangesets`, `SqlBookmarks`, etc.  This crate provides common utilities for constructing
//! these database managers, backed by various database types.
//!
//! Database managers should implement `SqlConstruct` to define how to be constructed from
//! a set of `SqlConnections`.  This is sufficient to allow construction based on `DatabaseConfig`,
//! which is provided through the `SqlConstructFromDatabaseConfig` trait.
//!
//! Database managers that support sharding should additionally implement `SqlShardedConstruct` for
//! the sharded case.
//!
//! Database managers that would like to be constructed from repository metadata configuration
//! should implement the `SqlConstructFromMetadataDatabaseConfig` trait.  If their data is not
//! stored in the primary metadata database, they should implement the `remote_database_config`
//! method to define which configuration is used for remote database configuration.
//!
//! Database managers that support sharding should instead implement the
//! `SqlShardableConstructFromMetadataDatabaseConfig` trait, which allows them to return
//! either sharded or unsharded configuration from `remote_database_config`.

mod config;
mod construct;

pub use config::{
    SqlConstructFromDatabaseConfig, SqlConstructFromMetadataDatabaseConfig,
    SqlShardableConstructFromMetadataDatabaseConfig,
};
pub use construct::{SqlConstruct, SqlShardedConstruct};

pub mod facebook {

    pub use r#impl::*;

    #[cfg(fbcode_build)]
    mod r#impl;

    #[cfg(not(fbcode_build))]
    mod r#impl {
        use crate::{SqlConstruct, SqlShardedConstruct};

        use anyhow::Result;
        use async_trait::async_trait;
        use fbinit::FacebookInit;
        use sql_ext::facebook::{MysqlOptions, ReadConnectionType};

        macro_rules! fb_unimplemented {
            () => {
                unimplemented!("This is implemented only for fbcode_build!")
            };
        }

        /// Construct a SQL data manager backed by Facebook infrastructure
        #[async_trait]
        pub trait FbSqlConstruct: SqlConstruct + Sized + Send + Sync + 'static {
            fn with_myrouter(_: String, _: u16, _: ReadConnectionType, _: bool) -> Self {
                fb_unimplemented!()
            }

            async fn with_raw_xdb_tier(
                _: FacebookInit,
                _: String,
                _: ReadConnectionType,
                _: bool,
            ) -> Result<Self> {
                fb_unimplemented!()
            }

            async fn with_xdb(
                _: FacebookInit,
                _: String,
                _: MysqlOptions,
                _: bool,
            ) -> Result<Self> {
                fb_unimplemented!()
            }
        }

        impl<T: SqlConstruct> FbSqlConstruct for T {}

        /// Construct a sharded SQL data manager backed by Facebook infrastructure
        #[async_trait]
        pub trait FbSqlShardedConstruct:
            SqlShardedConstruct + Sized + Send + Sync + 'static
        {
            fn with_sharded_myrouter(
                _: String,
                _: usize,
                _: u16,
                _: ReadConnectionType,
                _: bool,
            ) -> Self {
                fb_unimplemented!()
            }

            async fn with_sharded_raw_xdb_tier(
                _: FacebookInit,
                _: String,
                _: usize,
                _: ReadConnectionType,
                _: bool,
            ) -> Result<Self> {
                fb_unimplemented!()
            }

            async fn with_sharded_xdb(
                _: FacebookInit,
                _: String,
                _: usize,
                _: MysqlOptions,
                _: bool,
            ) -> Result<Self> {
                fb_unimplemented!()
            }
        }

        impl<T: SqlShardedConstruct> FbSqlShardedConstruct for T {}
    }
}
