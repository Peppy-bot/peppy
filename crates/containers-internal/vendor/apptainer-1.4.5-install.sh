#!/bin/bash
# Post-installation script for apptainer.
#
# This script is called by build.rs after RPM extraction. It expects to be
# run from within the architecture subdirectory (e.g. {install_dir}/x86_64/)
# which already contains the apptainer RPM extracted (usr/bin/apptainer, etc.)
# and a "tmp/" directory with extracted dependency RPMs.
#
# Based on install-unprivileged.sh from the Apptainer project (adapted for setuid mode).
# https://github.com/apptainer/apptainer

set -euo pipefail

fatal() { echo "FATAL: $*" >&2; exit 1; }

INSTALL_DIR="$1"  # Absolute path to the base install dir (parent of arch dir)
ARCH="$2"         # e.g. x86_64

ARCH_DIR="$INSTALL_DIR/$ARCH"
cd "$ARCH_DIR"

# -----------------------------------------------------------------------
# Phase 1: Flatten the apptainer RPM extraction (usr/* -> .)
# -----------------------------------------------------------------------
if [ ! -f usr/bin/apptainer ]; then
    fatal "Required file usr/bin/apptainer missing in $ARCH_DIR"
fi
mv usr/* .
rmdir usr

# -----------------------------------------------------------------------
# Phase 2: Remove .build-id files (present in el8+)
# -----------------------------------------------------------------------
rm -rf lib/.build-id
rmdir lib 2>/dev/null || true

# -----------------------------------------------------------------------
# Phase 3: Patch fakeroot-sysv to be relocatable
# -----------------------------------------------------------------------
cd tmp
if [ -f usr/bin/fakeroot-sysv ]; then
    echo "Patching fakeroot-sysv to make it relocatable"
    # shellcheck disable=SC2016
    sed -i \
        -e 's,^FAKEROOT_PREFIX=/.*,FAKEROOT_BINDIR=${0%/*},' \
        -e 's,FAKEROOT_BINDIR=/.*,FAKEROOT_PREFIX=${FAKEROOT_BINDIR%/*},' \
        -e 's,^PATHS=/usr/lib[^/]*/libfakeroot:,PATHS=,' \
        -e 's,/lib32/,/lib/,' \
        usr/bin/fakeroot-sysv
fi
cd ..

# -----------------------------------------------------------------------
# Phase 4: Create utils directory structure from dependency RPMs
# -----------------------------------------------------------------------
mkdir -p utils/bin utils/lib utils/libexec

mv tmp/usr/lib*/* utils/lib
mv tmp/lib*/* utils/lib 2>/dev/null || true  # optional
mv tmp/usr/*bin/*squashfs utils/libexec
mv tmp/usr/*bin/fuse* utils/libexec 2>/dev/null || true  # optional
mv tmp/usr/bin/fake*sysv utils/bin

# Create the utils wrapper script
cat >utils/bin/.wrapper <<'WRAPPER'
#!/bin/bash
BASEME=${0##*/}
HERE="${0%/*}"
if [ "$HERE" = "." ]; then
	HERE="$PWD"
elif [[ "$HERE" != /* ]]; then
	HERE="$PWD/$HERE"
fi
PARENT="${HERE%/*}"
#_WRAPPER_EXEC_CMD and _WRAPPER_ARG0 are sometimes used by apptainer
REALME=$PARENT/libexec/$BASEME
ARG0="${_WRAPPER_ARG0:-$REALME}"
LD_LIBRARY_PATH=$PARENT/lib ${_WRAPPER_EXEC_CMD:-exec -a "$ARG0"} $REALME "$@"
WRAPPER
chmod +x utils/bin/.wrapper

for TOOL in utils/libexec/*; do
    ln -s .wrapper "utils/bin/${TOOL##*/}"
done

rm -rf tmp

# -----------------------------------------------------------------------
# Phase 5: Create wrappers for libexec/apptainer/bin scripts
#
# starter and starter-suid are kept as real ELF binaries (not wrapped):
# - starter-suid needs its setuid bit preserved (Linux ignores setuid on scripts)
# - LD_LIBRARY_PATH is stripped by the kernel for setuid binaries
# - Both link against system libc/libseccomp and do not need vendored libs
# -----------------------------------------------------------------------
mkdir -p libexec/apptainer/libexec

cat >libexec/apptainer/bin/.wrapper <<'WRAPPER'
#!/bin/bash
BASEME=${0##*/}
HERE="${0%/*}"
if [ "$HERE" = "." ]; then
	HERE="$PWD"
elif [[ "$HERE" != /* ]]; then
	HERE="$PWD/$HERE"
fi
PARENT="${HERE%/*}"
GGPARENT="${PARENT%/*/*}"
REALME=$PARENT/libexec/$BASEME
ARG0="${_WRAPPER_ARG0:-$REALME}"
LD_LIBRARY_PATH=$GGPARENT/utils/lib PATH=$GGPARENT/utils/bin:$PATH ${_WRAPPER_EXEC_CMD:-exec -a "$ARG0"} $REALME "$@"
WRAPPER
chmod +x libexec/apptainer/bin/.wrapper

for TOOL in libexec/apptainer/bin/*; do
    BASENAME="${TOOL##*/}"
    if [ "$BASENAME" = "starter" ] || [ "$BASENAME" = "starter-suid" ]; then
        continue
    fi
    mv "$TOOL" libexec/apptainer/libexec
    ln -s .wrapper "$TOOL"
done

# -----------------------------------------------------------------------
# Phase 6: Create top-level bin/apptainer launcher
# -----------------------------------------------------------------------
cd "$INSTALL_DIR"
mkdir -p bin
cat >bin/apptainer <<'LAUNCHER'
#!/bin/bash
ME="$(/usr/bin/realpath $0)"
HERE="${ME%/*}"
BASEPATH="${HERE%/*}"
ARCH="$(uname -m)"
if [ -z "$ARCH" ]; then
	echo "$0: cannot determine arch" 2>&1
	exit 1
fi
APPTDIR="$BASEPATH/$ARCH"
if [ ! -d "$APPTDIR" ]; then
	echo "$0: $APPTDIR not found" 2>&1
	exit 1
fi
if [ -n "$LD_LIBRARY_PATH" ]; then
	LD_LIBRARY_PATH="$LD_LIBRARY_PATH:"
fi
PATH=$APPTDIR/utils/bin:$PATH LD_LIBRARY_PATH="$LD_LIBRARY_PATH$APPTDIR/utils/lib" exec $APPTDIR/bin/apptainer "$@"
LAUNCHER
chmod +x bin/apptainer
ln -sf apptainer bin/singularity

echo "Post-installation complete in $INSTALL_DIR"
