// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

pub use failure::{Error, Result, ResultExt};

use mercurial_types::{HgNodeHash, RepoPath};

#[derive(Debug, Fail)]
pub enum ErrorKind {
    #[fail(display = "internal error: file {} copied from directory {}", _0, _1)]
    InconsistentCopyInfo(RepoPath, RepoPath),
    #[fail(display = "internal error: streaming blob {} missing", _0)]
    MissingStreamingBlob(String),
    #[fail(
        display = "Data corruption for {}: expected {}, actual {}!",
        _0, _1, _2
    )]
    DataCorruption {
        path: RepoPath,
        expected: HgNodeHash,
        actual: HgNodeHash,
    },
    #[fail(display = "Repo is marked as read-only")]
    RepoReadOnly,
}
