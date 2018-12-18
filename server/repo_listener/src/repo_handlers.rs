// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::HashMap;
use std::sync::Arc;

use failure::prelude::*;
use futures::{Future, future::{self, ok}};
use futures_ext::{BoxFuture, FutureExt};
use slog::Logger;
use sql::myrouter;

use blobstore::Blobstore;
use cache_warmup::cache_warmup;
use context::CoreContext;
use hooks::{HookManager, hook_loader::load_hooks};
use mercurial_types::RepositoryId;
use metaconfig::repoconfig::{RepoConfig, RepoType};
use phases::{CachingHintPhases, HintPhases, Phases, SqlConstructors, SqlPhases};
use reachabilityindex::{deserialize_skiplist_map, LeastCommonAncestorsHint, SkiplistIndex};
use ready_state::ReadyStateBuilder;
use repo_client::{open_blobrepo, streaming_clone, MononokeRepo};
use scuba_ext::{ScubaSampleBuilder, ScubaSampleBuilderExt};

#[derive(Clone)]
pub struct RepoHandler {
    pub logger: Logger,
    pub scuba: ScubaSampleBuilder,
    pub wireproto_scribe_category: Option<String>,
    pub repo: MononokeRepo,
    pub hash_validation_percentage: usize,
    pub lca_hint: Arc<LeastCommonAncestorsHint + Send + Sync>,
    pub phases_hint: Arc<Phases + Send + Sync>,
}

pub fn repo_handlers(
    repos: impl IntoIterator<Item = (String, RepoConfig)>,
    myrouter_port: Option<u16>,
    root_log: &Logger,
    ready: &mut ReadyStateBuilder,
) -> BoxFuture<HashMap<String, RepoHandler>, Error> {
    // compute eagerly to avoid lifetime issues
    let repos: Vec<_> = repos
        .into_iter()
        .filter(|(reponame, config)| {
            if !config.enabled {
                info!(root_log, "Repo {} not enabled", reponame)
            };
            config.enabled
        })
        .map(|(reponame, config)| {
            info!(
                root_log,
                "Start warming for repo {}, type {:?}", reponame, config.repotype
            );
            // TODO(T37478150, luk): this is not a test use case, need to address this later
            let ctx = CoreContext::test_mock();
            let ensure_myrouter_ready = match config.get_db_address() {
                None => future::ok(()).left_future(),
                Some(db_address) => {
                    let myrouter_port = try_boxfuture!(myrouter_port.ok_or_else(|| format_err!(
                        "No port for MyRouter provided, but repo {} needs to connect do db {}",
                        reponame,
                        db_address
                    )));
                    myrouter::wait_for_myrouter(myrouter_port, db_address).right_future()
                }
            };

            let ready_handle = ready.create_handle(reponame.as_ref());

            let logger = root_log.new(o!("repo" => reponame.clone()));
            let repoid = RepositoryId::new(config.repoid);
            let blobrepo = try_boxfuture!(open_blobrepo(
                logger.clone(),
                config.repotype.clone(),
                repoid,
                myrouter_port,
            ));

            let hook_manager_params = match config.hook_manager_params.clone() {
                Some(hook_manager_params) => hook_manager_params,
                None => Default::default(),
            };

            let mut hook_manager = HookManager::new_with_blobrepo(
                ctx.clone(),
                hook_manager_params,
                blobrepo.clone(),
                logger,
            );

            info!(root_log, "Loading hooks");
            try_boxfuture!(load_hooks(&mut hook_manager, config.clone()));

            let streaming_clone = match config.repotype {
                RepoType::BlobRemote { ref db_address, .. } => {
                    Some(try_boxfuture!(streaming_clone(
                        blobrepo.clone(),
                        &db_address,
                        myrouter_port.expect("myrouter_port not provided for BlobRemote repo"),
                        repoid
                    )))
                }
                _ => None,
            };

            let repo = MononokeRepo::new(
                blobrepo,
                &config.pushrebase,
                Arc::new(hook_manager),
                streaming_clone,
                config.lfs.clone(),
                reponame.clone(),
                config.readonly,
            );

            let listen_log = root_log.new(o!("repo" => reponame.clone()));
            let mut scuba_logger = ScubaSampleBuilder::with_opt_table(config.scuba_table.clone());
            scuba_logger.add_common_server_data();
            let hash_validation_percentage = config.hash_validation_percentage.clone();
            let wireproto_scribe_category = config.wireproto_scribe_category.clone();

            let lca_hint = match config.skiplist_index_blobstore_key.clone() {
                Some(skiplist_index_blobstore_key) => {
                    let blobstore = repo.blobrepo().get_blobstore();
                    blobstore
                        .get(ctx.clone(), skiplist_index_blobstore_key)
                        .and_then(|maybebytes| {
                            let map = match maybebytes {
                                Some(bytes) => {
                                    let bytes = bytes.into_bytes();
                                    try_boxfuture!(deserialize_skiplist_map(bytes))
                                }
                                None => HashMap::new(),
                            };
                            ok(Arc::new(SkiplistIndex::new_with_skiplist_graph(map))).boxify()
                        })
                        .left_future()
                }
                None => ok(Arc::new(SkiplistIndex::new())).right_future(),
            };

            let phases_hint: Arc<Phases + Send + Sync> = match config.repotype {
                RepoType::BlobFiles(ref data_dir)
                | RepoType::BlobRocks(ref data_dir)
                | RepoType::TestBlobDelayRocks(ref data_dir, ..) => {
                    let storage = Arc::new(
                        SqlPhases::with_sqlite_path(data_dir.join("phases"))
                            .expect("unable to initialize sqlite db for phases"),
                    );
                    Arc::new(HintPhases::new(storage))
                }
                RepoType::BlobRemote { ref db_address, .. } => {
                    let storage = Arc::new(SqlPhases::with_myrouter(
                        &db_address,
                        myrouter_port.expect("myrouter_port not provided for BlobRemote repo"),
                    ));
                    Arc::new(CachingHintPhases::new(storage))
                }
            };

            // TODO (T32873881): Arc<BlobRepo> should become BlobRepo
            let initial_warmup = ensure_myrouter_ready.and_then({
                cloned!(ctx, reponame, listen_log);
                let blobrepo = repo.blobrepo().clone();
                move |()| {
                    cache_warmup(ctx, Arc::new(blobrepo), config.cache_warmup, listen_log)
                        .chain_err(format!("while warming up cache for repo: {}", reponame))
                        .from_err()
                }
            });

            ready_handle
                .wait_for(initial_warmup.join(lca_hint).map(|((), lca_hint)| lca_hint))
                .map({
                    cloned!(root_log);
                    move |lca_hint| {
                        info!(root_log, "Repo warmup for {} complete", reponame);
                        (
                            reponame,
                            RepoHandler {
                                logger: listen_log,
                                scuba: scuba_logger,
                                wireproto_scribe_category,
                                repo,
                                hash_validation_percentage,
                                lca_hint,
                                phases_hint,
                            },
                        )
                    }
                })
                .boxify()
        })
        .collect();

    future::join_all(repos)
        .map(|repos| repos.into_iter().collect())
        .boxify()
}
