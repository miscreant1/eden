# stackpush - specialized pushrebase
#
# Copyright 2018 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.
"""
push a stack of linear commits to the destination.

Typically a push looks like this:

  F onto bookmark (in critical section)
  .
  .
  E onto bookmark (outside critical section)
  .
  . D stack top
  | .
  | .
  | C
  | |
  | B stack bottom
  |/
  A stack parent

Pushrebase would need to check files changed in B::D are not touched in A::F.

stackpush tries to minimize steps inside the critical section:

  1. Avoid constructing a bundle repo in the critical section.
     Instead, collect all the data needed for *checking* and pushing B::D
     beforehand. That is, a {path: old_filenode} map for checking, and
     [(commit_metadata, {path: new_file})] for pushing.
  2. Only check F's manifest for the final decision for conflicts.
     Do not read E::F in the critical section.
"""

from __future__ import absolute_import

import time

from edenscm.mercurial import context, error, mutation
from edenscm.mercurial.node import hex, nullid, nullrev

from .common import commitdategenerator
from .errors import ConflictsError, StackPushUnsupportedError


class pushcommit(object):
    def __init__(self, orignode, user, date, desc, extra, filechanges, examinepaths):
        self.orignode = orignode
        self.user = user
        self.date = date
        self.desc = desc
        self.extra = extra
        self.filechanges = filechanges  # {path: (mode, content, copysource) | None}
        self.examinepaths = examinepaths  # {path}

    @classmethod
    def fromctx(cls, ctx):
        filechanges = {}
        examinepaths = set(ctx.files())
        # Preload the manifest since we know we'll be inspecting it many times.
        # Otherwise it may take fast paths that are efficient at a low number of
        # files, but very expensive at a high number of files.
        ctx.manifest()
        for path in ctx.files():
            try:
                fctx = ctx[path]
            except error.ManifestLookupError:
                filechanges[path] = None
            else:
                if fctx.rawflags():
                    raise StackPushUnsupportedError("stackpush does not support LFS")
                renamed = fctx.renamed()
                if renamed:
                    copysource = renamed[0]
                    examinepaths.add(copysource)
                else:
                    copysource = None
                filechanges[path] = (fctx.flags(), fctx.data(), copysource)
        return cls(
            ctx.node(),
            ctx.user(),
            ctx.date(),
            ctx.description(),
            ctx.extra(),
            filechanges,
            examinepaths,
        )


class pushrequest(object):
    def __init__(self, stackparentnode, pushcommits, fileconditions):
        self.stackparentnode = stackparentnode
        self.pushcommits = pushcommits
        self.fileconditions = fileconditions  # {path: None | filenode}

    @classmethod
    def fromrevset(cls, repo, spec):
        """Construct a pushrequest from revset"""
        # No merge commits allowed.
        revs = list(repo.revs(spec))
        if repo.revs("%ld and merge()", revs):
            raise StackPushUnsupportedError("stackpush does not support merges")
        parentrevs = list(repo.revs("parents(%ld)-%ld", revs, revs))
        if len(parentrevs) > 1:
            raise StackPushUnsupportedError(
                "stackpush only supports single linear stack"
            )

        examinepaths = set()

        # calculate "pushcommit"s, and paths to examine
        pushcommits = []
        for rev in revs:
            ctx = repo[rev]
            commit = pushcommit.fromctx(ctx)
            examinepaths.update(commit.examinepaths)
            pushcommits.append(commit)

        # calculate "fileconditions" - filenodes in the signal parent commit
        parentctx = repo[(parentrevs + [nullrev])[0]]
        parentmanifest = parentctx.manifestctx()
        fileconditions = {}
        for path in examinepaths:
            try:
                filenodemode = parentmanifest.find(path)
            except KeyError:
                filenodemode = None
            fileconditions[path] = filenodemode

        return cls(parentctx.node(), pushcommits, fileconditions)

    def pushonto(self, ctx):
        """Push the stack onto ctx

        Return (added, replacements)
        """
        self.check(ctx)
        return self._pushunchecked(ctx)

    def check(self, ctx):
        """Check if push onto ctx can be done

        Raise ConflictsError if there are conflicts.
        """
        mctx = ctx.manifestctx()
        conflicts = []
        for path, expected in self.fileconditions.iteritems():
            try:
                actual = mctx.find(path)
            except KeyError:
                actual = None
            if actual != expected:
                conflicts.append(path)
        if conflicts:
            raise ConflictsError(conflicts)

    def _pushunchecked(self, ctx):
        added = []
        replacements = {}
        repo = ctx.repo()
        getcommitdate = commitdategenerator(repo.ui)
        for commit in self.pushcommits:
            newnode = self._pushsingleunchecked(ctx, commit, getcommitdate)
            added.append(newnode)
            replacements[commit.orignode] = newnode
            ctx = repo[newnode]
        return added, replacements

    @staticmethod
    def _pushsingleunchecked(ctx, commit, getcommitdate):
        """Return newly pushed node"""
        repo = ctx.repo()

        date = getcommitdate(repo.ui, hex(commit.orignode), commit.date)

        def getfilectx(repo, memctx, path):
            assert path in commit.filechanges
            entry = commit.filechanges[path]
            if entry is None:
                # deleted
                return None
            else:
                # changed or created
                mode, content, copysource = entry
                return context.memfilectx(
                    repo,
                    memctx,
                    path,
                    content,
                    islink=("l" in mode),
                    isexec=("x" in mode),
                    copied=copysource,
                )

        extra = commit.extra.copy()
        mutation.record(repo, extra, [commit.orignode], "pushrebase")

        return context.memctx(
            repo,
            [ctx.node(), nullid],
            commit.desc,
            sorted(commit.filechanges),
            getfilectx,
            commit.user,
            date,
            extra,
        ).commit()
