# Copyright 2017 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

import json
import os
import struct
import tempfile

from edenscm.mercurial import error, extensions, util
from edenscm.mercurial.node import hex


def isserver(ui):
    return ui.configbool("infinitepush", "server")


def reposetup(ui, repo):
    repo._scratchbranchmatcher = scratchbranchmatcher(ui)


def isremotebooksenabled(ui):
    return "remotenames" in extensions._extensions and ui.configbool(
        "remotenames", "bookmarks"
    )


def encodebookmarks(bookmarks):
    encoded = {}
    for bookmark, node in bookmarks.iteritems():
        encoded[bookmark] = node
    dumped = json.dumps(encoded)
    result = struct.pack(">i", len(dumped)) + dumped
    return result


def downloadbundle(repo, unknownbinhead):
    index = repo.bundlestore.index
    store = repo.bundlestore.store
    bundleid = index.getbundle(hex(unknownbinhead))
    if bundleid is None:
        raise error.Abort("%s head is not known" % hex(unknownbinhead))
    bundleraw = store.read(bundleid)
    return _makebundlefromraw(bundleraw)


def _makebundlefromraw(data):
    fp = None
    fd, bundlefile = tempfile.mkstemp()
    try:  # guards bundlefile
        try:  # guards fp
            fp = os.fdopen(fd, "wb")
            fp.write(data)
        finally:
            fp.close()
    except Exception:
        try:
            os.unlink(bundlefile)
        except Exception:
            # we would rather see the original exception
            pass
        raise

    return bundlefile


class scratchbranchmatcher(object):
    def __init__(self, ui):
        scratchbranchpat = ui.config("infinitepush", "branchpattern")
        if scratchbranchpat:
            _, _, matchfn = util.stringmatcher(scratchbranchpat)
        else:
            matchfn = lambda x: False
        self._matchfn = matchfn

    def match(self, bookmark):
        return self._matchfn(bookmark)
