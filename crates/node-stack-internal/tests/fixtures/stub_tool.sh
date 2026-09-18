#!/bin/sh
# Stands in for a node's `build_cmd` program in
# `run_build_cmd_resolves_the_program_via_the_child_path`. The test symlinks
# this file into a directory it hands the child as its entire PATH, so being
# reached at all is what the test is about; what this does is succeed. The
# test's doc comment says why the stub is a file the repository carries.
exit 0
