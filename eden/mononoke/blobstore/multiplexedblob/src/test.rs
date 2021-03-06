/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::base::{MultiplexedBlobstoreBase, MultiplexedBlobstorePutHandler};
use crate::queue::MultiplexedBlobstore;
use crate::scrub::{LoggingScrubHandler, ScrubBlobstore, ScrubHandler};
use anyhow::{bail, Error};
use blobstore::Blobstore;
use blobstore_sync_queue::{BlobstoreSyncQueue, OperationKey, SqlBlobstoreSyncQueue};
use bytes::Bytes;
use cloned::cloned;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::{
    compat::Future01CompatExt,
    future::FutureExt as _,
    task::{Context, Poll},
};
use futures_ext::{BoxFuture, FutureExt};
use futures_old::future::{Future, IntoFuture};
use futures_old::sync::oneshot;
use lock_ext::LockExt;
use memblob::LazyMemblob;
use metaconfig_types::{BlobstoreId, MultiplexId, ScrubAction};
use mononoke_types::BlobstoreBytes;
use nonzero_ext::nonzero;
use readonlyblob::ReadOnlyBlobstore;
use scuba::ScubaSampleBuilder;
use sql_construct::SqlConstruct;

pub struct Tickable<T> {
    pub storage: Arc<Mutex<HashMap<String, T>>>,
    // queue of pending operations
    queue: Arc<Mutex<VecDeque<oneshot::Sender<Option<String>>>>>,
}

impl<T: fmt::Debug> fmt::Debug for Tickable<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tickable")
            .field("storage", &self.storage)
            .field("pending", &self.queue.with(|q| q.len()))
            .finish()
    }
}

impl<T> Tickable<T> {
    pub fn new() -> Self {
        Self {
            storage: Default::default(),
            queue: Default::default(),
        }
    }

    // Broadcast either success or error to a set of outstanding futures, advancing the
    // overall state by one tick.
    pub fn tick(&self, error: Option<&str>) {
        let mut queue = self.queue.lock().unwrap();
        for send in queue.drain(..) {
            send.send(error.map(String::from)).unwrap();
        }
    }

    // Register this task on the tick queue and wait for it to progress.
    pub fn on_tick(&self) -> impl Future<Item = (), Error = Error> {
        let (send, recv) = oneshot::channel();
        let mut queue = self.queue.lock().unwrap();
        queue.push_back(send);
        recv.map_err(Error::from).and_then(|error| match error {
            None => Ok(()),
            Some(error) => bail!(error),
        })
    }
}

impl Blobstore for Tickable<BlobstoreBytes> {
    fn get(&self, _ctx: CoreContext, key: String) -> BoxFuture<Option<BlobstoreBytes>, Error> {
        let storage = self.storage.clone();
        self.on_tick()
            .map(move |_| storage.with(|s| s.get(&key).cloned()))
            .boxify()
    }

    fn put(&self, _ctx: CoreContext, key: String, value: BlobstoreBytes) -> BoxFuture<(), Error> {
        let storage = self.storage.clone();
        self.on_tick()
            .map(move |_| {
                storage.with(|s| {
                    s.insert(key, value);
                })
            })
            .boxify()
    }
}

impl MultiplexedBlobstorePutHandler for Tickable<BlobstoreId> {
    fn on_put(
        &self,
        _ctx: CoreContext,
        blobstore_id: BlobstoreId,
        _multiplex_id: MultiplexId,
        _operation_key: OperationKey,
        key: String,
    ) -> BoxFuture<(), Error> {
        let storage = self.storage.clone();
        self.on_tick()
            .map(move |_| {
                storage.with(|s| {
                    s.insert(key, blobstore_id);
                })
            })
            .boxify()
    }
}

struct LogHandler {
    pub log: Arc<Mutex<Vec<(BlobstoreId, String)>>>,
}

impl LogHandler {
    fn new() -> Self {
        Self {
            log: Default::default(),
        }
    }
    fn clear(&self) {
        self.log.with(|log| log.clear())
    }
}

impl MultiplexedBlobstorePutHandler for LogHandler {
    fn on_put(
        &self,
        _ctx: CoreContext,
        blobstore_id: BlobstoreId,
        _multiplex_id: MultiplexId,
        _operation_key: OperationKey,
        key: String,
    ) -> BoxFuture<(), Error> {
        self.log.with(move |log| log.push((blobstore_id, key)));
        Ok(()).into_future().boxify()
    }
}

fn make_value(value: &str) -> BlobstoreBytes {
    BlobstoreBytes::from_bytes(Bytes::copy_from_slice(value.as_bytes()))
}

#[fbinit::test]
fn base(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let waker = futures::task::noop_waker();
        let mut task_ctx = Context::from_waker(&waker);

        let bs0 = Arc::new(Tickable::new());
        let bs1 = Arc::new(Tickable::new());
        let log = Arc::new(LogHandler::new());
        let bs = MultiplexedBlobstoreBase::new(
            MultiplexId::new(1),
            vec![
                (BlobstoreId::new(0), bs0.clone()),
                (BlobstoreId::new(1), bs1.clone()),
            ],
            log.clone(),
            ScubaSampleBuilder::with_discard(),
            nonzero!(1u64),
        );
        let ctx = CoreContext::test_mock(fb);

        // succeed as soon as first blobstore succeeded
        {
            let v0 = make_value("v0");
            let k0 = String::from("k0");

            let mut put_fut = bs
                .put(ctx.clone(), k0.clone(), v0.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            put_fut.await.unwrap();
            assert_eq!(bs0.storage.with(|s| s.get(&k0).cloned()), Some(v0.clone()));
            assert!(bs1.storage.with(|s| s.is_empty()));
            bs1.tick(Some("bs1 failed"));
            assert!(log
                .log
                .with(|log| log == &vec![(BlobstoreId::new(0), k0.clone())]));

            // should succeed as it is stored in bs1
            let mut get_fut = bs.get(ctx.clone(), k0).map_err(|_| ()).compat().boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            bs1.tick(None);
            assert_eq!(get_fut.await.unwrap(), Some(v0));
            assert!(bs1.storage.with(|s| s.is_empty()));

            log.clear();
        }

        // wait for second if first one failed
        {
            let v1 = make_value("v1");
            let k1 = String::from("k1");

            let mut put_fut = bs
                .put(ctx.clone(), k1.clone(), v1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("case 2: bs0 failed"));
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs1.tick(None);
            put_fut.await.unwrap();
            assert!(bs0.storage.with(|s| s.get(&k1).is_none()));
            assert_eq!(bs1.storage.with(|s| s.get(&k1).cloned()), Some(v1.clone()));
            assert!(log
                .log
                .with(|log| log == &vec![(BlobstoreId::new(1), k1.clone())]));

            let mut get_fut = bs
                .get(ctx.clone(), k1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs1.tick(None);
            assert_eq!(get_fut.await.unwrap(), Some(v1));
            assert!(bs0.storage.with(|s| s.get(&k1).is_none()));

            log.clear();
        }

        // both fail => whole put fail
        {
            let k2 = String::from("k2");
            let v2 = make_value("v2");

            let mut put_fut = bs
                .put(ctx.clone(), k2.clone(), v2.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("case 3: bs0 failed"));
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs1.tick(Some("case 3: bs1 failed"));
            assert!(put_fut.await.is_err());
        }

        // get: Error + None -> Error
        {
            let k3 = String::from("k3");
            let mut get_fut = bs.get(ctx.clone(), k3).map_err(|_| ()).compat().boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(Some("case 4: bs0 failed"));
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(None);
            assert!(get_fut.await.is_err());
        }

        // get: None + None -> None
        {
            let k3 = String::from("k3");
            let mut get_fut = bs.get(ctx.clone(), k3).map_err(|_| ()).compat().boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(None);
            assert_eq!(get_fut.await.unwrap(), None);
        }

        // both put succeed
        {
            let k4 = String::from("k4");
            let v4 = make_value("v4");
            log.clear();

            let mut put_fut = bs
                .put(ctx.clone(), k4.clone(), v4.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            put_fut.await.unwrap();
            assert_eq!(bs0.storage.with(|s| s.get(&k4).cloned()), Some(v4.clone()));
            bs1.tick(None);
            while log.log.with(|log| log.len() != 2) {}
            assert_eq!(bs1.storage.with(|s| s.get(&k4).cloned()), Some(v4.clone()));
        }
    });
}

#[fbinit::test]
fn multiplexed(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let waker = futures::task::noop_waker();
        let mut task_ctx = Context::from_waker(&waker);

        let ctx = CoreContext::test_mock(fb);
        let queue = Arc::new(SqlBlobstoreSyncQueue::with_sqlite_in_memory().unwrap());

        let bid0 = BlobstoreId::new(0);
        let bs0 = Arc::new(Tickable::new());
        let bid1 = BlobstoreId::new(1);
        let bs1 = Arc::new(Tickable::new());
        let bs = MultiplexedBlobstore::new(
            MultiplexId::new(1),
            vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
            queue.clone(),
            ScubaSampleBuilder::with_discard(),
            nonzero!(1u64),
        );

        // non-existing key when one blobstore failing
        {
            let k0 = String::from("k0");

            let mut get_fut = bs
                .get(ctx.clone(), k0.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(Some("case 1: bs1 failed"));
            assert_eq!(get_fut.await.unwrap(), None);
        }

        // only replica containing key failed
        {
            let k1 = String::from("k1");
            let v1 = make_value("v1");

            let mut put_fut = bs
                .put(ctx.clone(), k1.clone(), v1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            bs1.tick(Some("case 2: bs1 failed"));
            put_fut.await.expect("case 2 put_fut failed");

            match queue
                .get(ctx.clone(), k1.clone())
                .compat()
                .await
                .expect("case 2 get failed")
                .as_slice()
            {
                [entry] => assert_eq!(entry.blobstore_id, bid0),
                _ => panic!("only one entry expected"),
            }

            let mut get_fut = bs
                .get(ctx.clone(), k1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("case 2: bs0 failed"));
            bs1.tick(None);
            assert!(get_fut.await.is_err());
        }

        // both replicas fail
        {
            let k2 = String::from("k2");

            let mut get_fut = bs
                .get(ctx.clone(), k2.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("case 3: bs0 failed"));
            bs1.tick(Some("case 3: bs1 failed"));
            assert!(get_fut.await.is_err());
        }
    });
}

#[fbinit::compat_test]
async fn multiplexed_operation_keys(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let queue = Arc::new(SqlBlobstoreSyncQueue::with_sqlite_in_memory().unwrap());

    let bid0 = BlobstoreId::new(0);
    let bs0 = Arc::new(LazyMemblob::new());
    let bid1 = BlobstoreId::new(1);
    let bs1 = Arc::new(LazyMemblob::new());
    let bid2 = BlobstoreId::new(2);
    // we need writes to fail there so there's something on the queue
    let bs2 = Arc::new(ReadOnlyBlobstore::new(LazyMemblob::new()));
    let bs = MultiplexedBlobstore::new(
        MultiplexId::new(1),
        vec![
            (bid0, bs0.clone()),
            (bid1, bs1.clone()),
            (bid2, bs2.clone()),
        ],
        queue.clone(),
        ScubaSampleBuilder::with_discard(),
        nonzero!(1u64),
    );

    // two replicas succeed, one fails the operation keys are equal and non-null
    {
        let k3 = String::from("k3");
        let v3 = make_value("v3");

        bs.put(ctx.clone(), k3.clone(), v3.clone())
            .map_err(|_| ())
            .compat()
            .await
            .expect("test multiplexed_operation_keys, put failed");

        match queue
            .get(ctx.clone(), k3.clone())
            .compat()
            .await
            .expect("test multiplexed_operation_keys, get failed")
            .as_slice()
        {
            [entry0, entry1] => {
                assert_eq!(entry0.operation_key, entry1.operation_key);
                assert!(!entry0.operation_key.is_null());
            }
            x => panic!(format!("two entries expected, got {:?}", x)),
        }
    }
    Ok(())
}

#[fbinit::test]
fn scrubbed(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let waker = futures::task::noop_waker();
        let mut task_ctx = Context::from_waker(&waker);

        let ctx = CoreContext::test_mock(fb);
        let queue = Arc::new(SqlBlobstoreSyncQueue::with_sqlite_in_memory().unwrap());
        let scrub_handler = Arc::new(LoggingScrubHandler::new(false)) as Arc<dyn ScrubHandler>;
        let bid0 = BlobstoreId::new(0);
        let bs0 = Arc::new(Tickable::new());
        let bid1 = BlobstoreId::new(1);
        let bs1 = Arc::new(Tickable::new());
        let bs = ScrubBlobstore::new(
            MultiplexId::new(1),
            vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
            queue.clone(),
            ScubaSampleBuilder::with_discard(),
            nonzero!(1u64),
            scrub_handler.clone(),
            ScrubAction::ReportOnly,
        );

        // non-existing key when one blobstore failing
        {
            let k0 = String::from("k0");

            let mut get_fut = bs
                .get(ctx.clone(), k0.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(Some("bs1 failed"));
            assert_eq!(get_fut.await.unwrap(), None, "None/Err no replication");
        }

        // only replica containing key failed
        {
            let k1 = String::from("k1");
            let v1 = make_value("v1");

            let mut put_fut = bs
                .put(ctx.clone(), k1.clone(), v1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(None);
            assert_eq!(put_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs1.tick(Some("bs1 failed"));
            put_fut.await.unwrap();

            match queue
                .get(ctx.clone(), k1.clone())
                .compat()
                .await
                .unwrap()
                .as_slice()
            {
                [entry] => assert_eq!(entry.blobstore_id, bid0, "Queue bad"),
                _ => panic!("only one entry expected"),
            }

            let mut get_fut = bs
                .get(ctx.clone(), k1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(Some("bs0 failed"));
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(None);
            assert!(get_fut.await.is_err(), "None/Err while replicating");
        }

        // both replicas fail
        {
            let k2 = String::from("k2");

            let mut get_fut = bs
                .get(ctx.clone(), k2.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("bs0 failed"));
            bs1.tick(Some("bs1 failed"));
            assert!(get_fut.await.is_err(), "Err/Err");
        }

        // Now replace bs1 with an empty blobstore, and see the scrub work
        let bid1 = BlobstoreId::new(1);
        let bs1 = Arc::new(Tickable::new());
        let bs = ScrubBlobstore::new(
            MultiplexId::new(1),
            vec![(bid0, bs0.clone()), (bid1, bs1.clone())],
            queue.clone(),
            ScubaSampleBuilder::with_discard(),
            nonzero!(1u64),
            scrub_handler,
            ScrubAction::Repair,
        );

        // Non-existing key in both blobstores, new blobstore failing
        {
            let k0 = String::from("k0");

            let mut get_fut = bs
                .get(ctx.clone(), k0.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs1.tick(Some("bs1 failed"));
            assert_eq!(get_fut.await.unwrap(), None, "None/Err after replacement");
        }

        // only replica containing key replaced after failure - DATA LOST
        {
            let k1 = String::from("k1");

            let mut get_fut = bs
                .get(ctx.clone(), k1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs0.tick(Some("bs0 failed"));
            bs1.tick(None);
            assert!(get_fut.await.is_err(), "Empty replacement against error");
        }

        // One working replica after failure.
        {
            let k1 = String::from("k1");
            let v1 = make_value("v1");

            match queue
                .get(ctx.clone(), k1.clone())
                .compat()
                .await
                .unwrap()
                .as_slice()
            {
                [entry] => {
                    assert_eq!(entry.blobstore_id, bid0, "Queue bad");
                    queue
                        .del(ctx.clone(), vec![entry.clone()])
                        .compat()
                        .await
                        .expect("Could not delete scrub queue entry");
                }
                _ => panic!("only one entry expected"),
            }

            // bs1 empty at this point
            assert_eq!(bs0.storage.with(|s| s.get(&k1).cloned()), Some(v1.clone()));
            assert!(bs1.storage.with(|s| s.is_empty()));

            let mut get_fut = bs
                .get(ctx.clone(), k1.clone())
                .map_err(|_| ())
                .compat()
                .boxed();
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            // tick the gets
            bs0.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            bs1.tick(None);
            assert_eq!(get_fut.poll_unpin(&mut task_ctx), Poll::Pending);
            // Tick the repairs
            bs1.tick(None);

            // Succeeds
            assert_eq!(get_fut.await.unwrap(), Some(v1.clone()));
            // Now both populated.
            assert_eq!(bs0.storage.with(|s| s.get(&k1).cloned()), Some(v1.clone()));
            assert_eq!(bs1.storage.with(|s| s.get(&k1).cloned()), Some(v1.clone()));
        }
    });
}

#[fbinit::test]
fn queue_waits(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let waker = futures::task::noop_waker();
        let mut task_ctx = Context::from_waker(&waker);

        let bs0 = Arc::new(Tickable::new());
        let bs1 = Arc::new(Tickable::new());
        let bs2 = Arc::new(Tickable::new());
        let log = Arc::new(Tickable::new());
        let bs = MultiplexedBlobstoreBase::new(
            MultiplexId::new(1),
            vec![
                (BlobstoreId::new(0), bs0.clone()),
                (BlobstoreId::new(1), bs1.clone()),
                (BlobstoreId::new(2), bs2.clone()),
            ],
            log.clone(),
            ScubaSampleBuilder::with_discard(),
            nonzero!(1u64),
        );
        let ctx = CoreContext::test_mock(fb);

        let clear = {
            cloned!(bs0, bs1, bs2, log);
            move || {
                bs0.tick(None);
                bs1.tick(None);
                bs2.tick(None);
                log.tick(None);
            }
        };

        let k = String::from("k");
        let v = make_value("v");

        // Put succeeds once all blobstores have succeded, even if the queue hasn't.
        {
            let mut fut = bs
                .put(ctx.clone(), k.clone(), v.clone())
                .map_err(|_| ())
                .compat()
                .boxed();

            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            bs1.tick(None);
            bs2.tick(None);

            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Ready(Ok(())));

            clear();
        }

        // Put succeeds after 1 write + a write to the queue
        {
            let mut fut = bs
                .put(ctx.clone(), k.clone(), v.clone())
                .map_err(|_| ())
                .compat()
                .boxed();

            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending);

            log.tick(None);
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Ready(Ok(())));

            clear();
        }

        // Put succeeds despite errors, if the queue succeeds
        {
            let mut fut = bs
                .put(ctx.clone(), k.clone(), v.clone())
                .map_err(|_| ())
                .compat()
                .boxed();

            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(None);
            bs1.tick(Some("oops"));
            bs2.tick(Some("oops"));
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending); // Trigger on_put

            log.tick(None);
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Ready(Ok(())));

            clear();
        }

        // Put succeeds if any blobstore succeeds and writes to the queue
        {
            let mut fut = bs
                .put(ctx.clone(), k.clone(), v.clone())
                .map_err(|_| ())
                .compat()
                .boxed();

            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending);

            bs0.tick(Some("oops"));
            bs1.tick(None);
            bs2.tick(Some("oops"));
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Pending); // Trigger on_put

            log.tick(None);
            assert_eq!(fut.poll_unpin(&mut task_ctx), Poll::Ready(Ok(())));

            clear();
        }
    });
}
