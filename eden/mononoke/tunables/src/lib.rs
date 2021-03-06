/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use cached_config::ConfigHandle;
use once_cell::sync::OnceCell;
use slog::{debug, warn, Logger};
use std::sync::atomic::AtomicI64;

use tunables_derive::Tunables;
use tunables_structs::Tunables as TunablesStruct;

static TUNABLES: OnceCell<MononokeTunables> = OnceCell::new();
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

pub fn tunables() -> &'static MononokeTunables {
    TUNABLES.get_or_init(MononokeTunables::default)
}

#[derive(Tunables, Default, Debug)]
pub struct MononokeTunables {
    warm_bookmark_cache_delay: AtomicI64,
}

pub fn init_tunables_worker(
    logger: Logger,
    conf_handle: ConfigHandle<TunablesStruct>,
) -> Result<()> {
    update_tunables(conf_handle.get())?;

    thread::Builder::new()
        .name("mononoke-tunables".into())
        .spawn({ move || worker(conf_handle, logger) })
        .expect("Can't spawn tunables updater");

    Ok(())
}

fn worker(config_handle: ConfigHandle<TunablesStruct>, logger: Logger) {
    loop {
        // TODO: Instead of refreshing tunables every loop iteration,
        // update cached_config to notify us when our config has changed.
        debug!(logger, "Refreshing tunables...");
        if let Err(e) = update_tunables(config_handle.get()) {
            warn!(logger, "Failed to refresh tunables: {}", e);
        }

        thread::sleep(REFRESH_INTERVAL);
    }
}

fn update_tunables(new_tunables: Arc<TunablesStruct>) -> Result<()> {
    let tunables = tunables();
    tunables.update_bools(&new_tunables.killswitches);
    tunables.update_ints(&new_tunables.ints);

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    #[derive(Tunables, Default)]
    struct TestTunables {
        boolean: AtomicBool,
        num: AtomicI64,
    }

    #[derive(Tunables, Default)]
    struct EmptyTunables {}

    #[test]
    fn test_empty_tunables() {
        let bools = HashMap::new();
        let ints = HashMap::new();
        let empty = EmptyTunables::default();

        empty.update_bools(&bools);
        empty.update_ints(&ints);
    }

    #[test]
    fn test_update_bool() {
        let mut d = HashMap::new();
        d.insert("boolean".to_string(), true);

        let test = TestTunables::default();
        assert_eq!(test.get_boolean(), false);
        test.update_bools(&d);
        assert_eq!(test.get_boolean(), true);
    }

    #[test]
    fn test_update_int() {
        let mut d = HashMap::new();
        d.insert("num".to_string(), 10);

        let test = TestTunables::default();
        assert_eq!(test.get_num(), 0);
        test.update_ints(&d);
        assert_eq!(test.get_num(), 10);
    }

    #[test]
    fn test_missing_int() {
        let mut d = HashMap::new();
        d.insert("missing".to_string(), 10);

        let test = TestTunables::default();
        assert_eq!(test.get_num(), 0);
        test.update_ints(&d);
        assert_eq!(test.get_num(), 0);
    }
}
