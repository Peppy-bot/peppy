#!/bin/sh
# Stands in for uv in the tests of the prune command of the uv cache
# (`node_stack::container_build_cache`). It prints the path it runs as, its
# arguments, and the uv variables it sees, so a test can tell which uv the
# command ran and with what environment. It uses shell builtins only, as the
# command runs it with an empty environment.
echo "ran: $0 $*"
echo "UV_LOCK_TIMEOUT=${UV_LOCK_TIMEOUT-unset} UV_NO_CACHE=${UV_NO_CACHE-unset} UV_CONFIG_FILE=${UV_CONFIG_FILE-unset}"
