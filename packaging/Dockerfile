# Runtime image for an already-built zdesk release — nothing is compiled here.
#
# The build context starts as an extracted release tarball (see
# build-tarball.sh), but its binary must be replaced with the output of
# build-container-binary.sh. That binary is built with no default features, so
# the image has the deployed gateway and never the managed-instance substrate.
# The Docker build verifies that boundary below instead of trusting its context.
# .github/workflows/release.yml wires this up; to do it by hand:
#
#   mkdir -p tmp/docker-context
#   tar -xzf zdesk-0.0.145-linux-x86_64.tar.gz -C tmp/docker-context --strip-components=1
#   bash packaging/build-container-binary.sh tmp/docker-context/bin/zdesk
#   docker build -f packaging/Dockerfile --build-arg VERSION=0.0.145 tmp/docker-context
#
# The base must provide every glibc symbol the binary needs. Release CI builds
# on ubuntu-26.04 runners and smoke-tests the result on this base; the runner's
# glibc version alone does not determine the binary's minimum. The v0.0.458
# amd64 and arm64 binaries require symbols through GLIBC_2.39, within trixie's
# 2.41. Keep the smoke test when changing the builder or the base.
FROM debian:trixie-slim

LABEL org.opencontainers.image.source=https://github.com/andrewtheguy/zdesk

# The RDP/VNC TLS paths accept self-signed server certificates, but rustls still
# wants a system trust store for anything conventionally signed.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The version is part of the on-disk layout, not decoration: zdesk derives its
# config path from its own location — <prefix>/versions/<version>/bin/zdesk
# resolves to <prefix>/etc/zdesk.toml and keeps state in <prefix>/var (see
# src/config.rs). That layout is what makes bare `serve` find
# /opt/zdesk/etc/zdesk.toml.
ARG VERSION
RUN test -n "${VERSION}" || { echo "the VERSION build-arg is required" >&2; exit 1; }

COPY bin/zdesk /opt/zdesk/versions/${VERSION}/bin/zdesk
# The example config only: the web client is compiled into the binary.
COPY share /opt/zdesk/versions/${VERSION}/share

# `current` is the stable path the entrypoint uses; canonicalizing it is how the
# binary finds the versioned root.
RUN ln -s "versions/${VERSION}" /opt/zdesk/current && mkdir -p /opt/zdesk/etc

# A normal native release binary has the embedded gateway feature enabled. Fail
# closed if one is accidentally placed in this context: release CI also checks,
# but keeping the invariant here covers every manual or downstream image build.
RUN set -eu; \
    binary=/opt/zdesk/current/bin/zdesk; \
    "$binary" --version >/dev/null; \
    case "$("$binary" --help)" in \
      *"  tui "*) echo "container binary exposes tui" >&2; exit 1 ;; \
    esac; \
    case "$("$binary" --help)" in \
      *serve-embedded*) echo "container binary exposes serve-embedded" >&2; exit 1 ;; \
    esac; \
    case "$("$binary" check-config --help)" in \
      *--embedded*) echo "container binary exposes check-config --embedded" >&2; exit 1 ;; \
    esac

# The config is the one thing that has to come from outside — bind-mount it
# read-only, it holds credentials (no VOLUME here: an anonymous volume for a
# config directory just accumulates):
#
#   docker run -p 52380:52380 -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro ...
#
# It must set [server] listen = "0.0.0.0:52380" — or the run must pass
# -e ZDESK_LISTEN=0.0.0.0:52380, which is the same setting from the one place a
# container has that is not the mounted file; the 127.0.0.1 default is
# unreachable from outside the container. A template ships at
# /opt/zdesk/current/share/doc/zdesk/zdesk.example.toml.
EXPOSE 52380

# zdesk installs its own SIGINT/SIGTERM handler (src/main.rs) and spawns no
# child processes, so it is fine as PID 1 — no init shim needed.
ENTRYPOINT ["/opt/zdesk/current/bin/zdesk"]
CMD ["serve"]
