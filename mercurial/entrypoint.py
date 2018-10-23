#!/usr/bin/env python

# mercurial - scalable distributed SCM
#
# Copyright 2005-2007 Matt Mackall <mpm@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.
from __future__ import absolute_import

import os
import sys


def run(binaryexecution):
    # entrypoint is in mercurial/ dir, while we want 'from mercurial import ...',
    # 'from hgext import ...' and 'from hgdemandimport import ...' to work
    # so we are adding their parent directory to be the first item of sys.path
    # Do not follow symlinks (ex. do not use "realpath"). It breaks buck build.
    filedir = os.path.dirname(os.path.abspath(__file__))
    libdir = os.path.dirname(filedir)
    if sys.path[0] != libdir:
        sys.path.insert(0, libdir)

    if filedir in sys.path:
        # the directory of entrypoint.py is mercurial/
        # and it should not be present in sys.path, as we use absolute_import
        sys.path.remove(filedir)

    import hgdemandimport

    hgdemandimport.tryenableembedded()

    from mercurial import encoding

    if encoding.environ.get("HGUNICODEPEDANTRY", False):
        try:
            reload(sys)
            sys.setdefaultencoding("undefined")
        except NameError:
            pass

    from mercurial import executionmodel

    executionmodel.setbinaryexecution(binaryexecution)

    if (
        sys.argv[1:5] == ["serve", "--cmdserver", "chgunix", "--address"]
        and sys.argv[6:8] == ["--daemon-postexec", "chdir:/"]
        and "CHGINTERNALMARK" in encoding.environ
    ):
        # Shortcut path for chg server
        from mercurial import dispatch

        dispatch.runchgserver()
    else:
        # Non-chg path
        try:
            if sys.version_info[0] < 3 or sys.version_info >= (3, 6):
                hgdemandimport.enable()
        except ImportError:
            sys.stderr.write(
                "abort: couldn't find mercurial libraries in [%s]\n"
                % " ".join(sys.path)
            )
            sys.stderr.write("(check your install and PYTHONPATH)\n")
            sys.exit(-1)
        from mercurial import dispatch

        dispatch.run()


if __name__ == "__main__":
    run(True)
