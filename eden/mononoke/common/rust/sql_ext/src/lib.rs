/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod sqlite;

use sql::{Connection, Transaction};

pub use sqlite::{open_sqlite_in_memory, open_sqlite_path};

#[derive(Clone)]
pub struct SqlConnections {
    pub write_connection: Connection,
    pub read_connection: Connection,
    pub read_master_connection: Connection,
}

impl SqlConnections {
    /// Create SqlConnections from a single connection.
    pub fn new_single(connection: Connection) -> Self {
        Self {
            write_connection: connection.clone(),
            read_connection: connection.clone(),
            read_master_connection: connection,
        }
    }
}

#[must_use]
pub enum TransactionResult {
    Succeeded(Transaction),
    Failed,
}

pub mod facebook {
    #[derive(Copy, Clone, Debug)]
    pub struct MysqlOptions {
        pub myrouter_port: Option<u16>,
        pub master_only: bool,
    }

    impl MysqlOptions {
        pub fn read_connection_type(&self) -> ReadConnectionType {
            if self.master_only {
                ReadConnectionType::Master
            } else {
                ReadConnectionType::Replica
            }
        }
    }

    #[derive(Copy, Clone, Debug)]
    pub enum ReadConnectionType {
        Replica,
        Master,
    }

    pub struct PoolSizeConfig {
        pub write_pool_size: usize,
        pub read_pool_size: usize,
        pub read_master_pool_size: usize,
    }

    pub use r#impl::*;

    #[cfg(fbcode_build)]
    mod r#impl;

    #[cfg(not(fbcode_build))]
    mod r#impl {
        use crate::{facebook::*, *};

        use anyhow::Error;
        use fbinit::FacebookInit;
        use futures_ext::BoxFuture;
        use slog::Logger;

        macro_rules! fb_unimplemented {
            () => {
                unimplemented!("This is implemented only for fbcode_build!")
            };
        }

        impl PoolSizeConfig {
            pub fn for_regular_connection() -> Self {
                fb_unimplemented!()
            }

            pub fn for_sharded_connection() -> Self {
                fb_unimplemented!()
            }
        }

        pub fn create_myrouter_connections(
            _: String,
            _: Option<usize>,
            _: u16,
            _: ReadConnectionType,
            _: PoolSizeConfig,
            _: String,
            _: bool,
        ) -> SqlConnections {
            fb_unimplemented!()
        }

        pub fn myrouter_ready(
            _: Option<String>,
            _: MysqlOptions,
            _: Logger,
        ) -> BoxFuture<(), Error> {
            fb_unimplemented!()
        }

        pub fn create_raw_xdb_connections(
            _: FacebookInit,
            _: String,
            _: ReadConnectionType,
            _: bool,
        ) -> BoxFuture<SqlConnections, Error> {
            fb_unimplemented!()
        }
    }
}
