# Runtime image for an already-built remotex release — nothing is compiled here.
#
# The build context is an *extracted release tarball* (see build-tarball.sh), so
# the image ships the exact binary and frontend bundle that were attached to the
# GitHub release: the frontend is built once for every platform, and each arch's
# image reuses that arch's release binary instead of rebuilding it.
# .github/workflows/release.yml wires this up; to do it by hand:
#
#   tar -xzf remotex-0.0.18-linux-x86_64.tar.gz
#   docker build -f packaging/Dockerfile --build-arg VERSION=0.0.18 remotex-0.0.18
#
# The base has to be at least as new as the builder: the release binaries are
# built on the ubuntu-24.04 runners (glibc 2.39), and trixie's 2.41 runs them
# fine — a base with an *older* glibc would not.
FROM debian:trixie-slim

LABEL org.opencontainers.image.source=https://github.com/andrewtheguy/remotex

# The RDP/VNC TLS paths accept self-signed server certificates, but rustls still
# wants a system trust store for anything conventionally signed.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The version is part of the on-disk layout, not decoration: remotex derives its
# config path from its own location — <prefix>/versions/<version>/bin/remotex
# resolves to <prefix>/etc/remotex.toml (see src/config.rs). Mirroring the real
# install layout is what makes bare `serve` find /opt/remotex/etc/remotex.toml.
ARG VERSION
RUN test -n "${VERSION}" || { echo "the VERSION build-arg is required" >&2; exit 1; }

COPY bin/remotex /opt/remotex/versions/${VERSION}/bin/remotex
COPY share /opt/remotex/versions/${VERSION}/share

# `current` is the stable path the entrypoint uses; canonicalizing it is how the
# binary finds the versioned root, exactly as with the tarball installer.
RUN ln -s "versions/${VERSION}" /opt/remotex/current && mkdir -p /opt/remotex/etc

# The config is the one thing that has to come from outside — bind-mount it
# read-only, it holds credentials (no VOLUME here: an anonymous volume for a
# config directory just accumulates):
#
#   docker run -p 52380:52380 -v ./remotex.toml:/opt/remotex/etc/remotex.toml:ro ...
#
# It must set [server] host = "0.0.0.0"; the 127.0.0.1 default is unreachable
# from outside the container. A template ships at
# /opt/remotex/current/share/doc/remotex/remotex.toml.example.
EXPOSE 52380

# remotex installs its own SIGINT/SIGTERM handler (src/main.rs) and spawns no
# child processes, so it is fine as PID 1 — no init shim needed.
ENTRYPOINT ["/opt/remotex/current/bin/remotex"]
CMD ["serve"]
