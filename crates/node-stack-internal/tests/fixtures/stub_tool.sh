#!/bin/sh
# Stands in for a node's `build_cmd` program in
# `run_build_cmd_resolves_the_program_via_the_child_path`. The test symlinks
# this file into a directory it hands the child as its entire PATH, so being
# reached at all is what the test is about; what this does is succeed.
#
# It is a file the repository carries rather than one the test writes, because
# a file this process has just written is still open for writing in every
# child a sibling test forked, until that child reaches its own `execve`, and
# executing it inside that window fails with `ETXTBSY`.
exit 0
