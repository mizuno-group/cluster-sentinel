# syntax=docker/dockerfile:1
#
# One image for every role in the pseudo-cluster.
#
# The roles differ only in which processes run, which is a supervisor
# configuration rather than a different filesystem. One image means one build,
# one layer cache, and no chance of the roles drifting apart in ways that have
# nothing to do with what is being tested.

# ---------------------------------------------------------------------------
# Build the sentinel binary against the same libc the runtime image uses.
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /build

# Dependencies first, so editing our own source does not rebuild the world.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release 2>/dev/null || true \
 && rm -rf src

COPY src ./src
COPY migrations ./migrations
RUN touch src/main.rs src/lib.rs && cargo build --release && strip target/release/sentinel

# ---------------------------------------------------------------------------
# Runtime.
#
# Ubuntu 24.04 rather than Debian bookworm, for one specific reason: bookworm
# ships Slurm 22.05, which has no `CgroupPlugin=disabled`. Without it slurmd
# cannot start in a container, because it tries to attach itself to a systemd
# scope over a dbus socket that does not exist, against a read-only cgroup2
# mount. 24.04 ships 23.11, where disabling the plugin outright is supported.
#
# The binary is built on bookworm and runs here: an older glibc build runs on a
# newer glibc, not the other way round.
# ---------------------------------------------------------------------------
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      munge \
      slurm-wlm \
      slurmctld \
      slurmd \
      slurm-client \
      openssh-server \
      supervisor \
      iproute2 \
      iptables \
      procps \
      netcat-openbsd \
      curl \
      ca-certificates \
      python3-minimal \
 && rm -rf /var/lib/apt/lists/*

# Slurm needs these to exist and be writable before it will start.
RUN mkdir -p /var/spool/slurmctld /var/spool/slurmd /var/log/slurm /run/munge \
             /var/lib/sentinel /etc/sentinel /run/sshd /var/log/supervisor \
 && chown -R slurm:slurm /var/spool/slurmctld /var/log/slurm

# Password login is disabled; the SSH probe only checks that the service
# answers, and never authenticates (SPEC.md §60).
RUN ssh-keygen -A \
 && sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin no/' /etc/ssh/sshd_config \
 && sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config

COPY --from=builder /build/target/release/sentinel /usr/local/bin/sentinel

COPY dev/compose/config/supervisor/ /etc/supervisor/roles/
COPY dev/compose/scripts/entrypoint /usr/local/bin/entrypoint
COPY dev/compose/scripts/storage-service /usr/local/bin/storage-service
RUN chmod +x /usr/local/bin/entrypoint /usr/local/bin/storage-service

ENTRYPOINT ["/usr/local/bin/entrypoint"]
