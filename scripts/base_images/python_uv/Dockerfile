FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# The Ubuntu archive snapshot that apt reads in this image and in every build
# on top of it. apt_snapshot.sh, of the build context scripts/base_images,
# installs the CA certificates and the packages named here from it.
ARG APT_SNAPSHOT
RUN --mount=type=bind,source=apt_snapshot.sh,target=/run/apt_snapshot.sh \
    sh /run/apt_snapshot.sh setup "$APT_SNAPSHOT" curl

RUN curl -LsSf https://astral.sh/uv/install.sh | sh

ENV PATH="/root/.local/bin:$PATH"

# Mount point for the peppy container build cache.
RUN mkdir /peppy-cache
