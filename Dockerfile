# testry development / verification image.
#
# Policy (docs/adr/0002-container-first-execution.md): every toolchain and
# runtime (cargo, rustc, git, bun, node) runs inside a container. Nothing is
# installed or executed on the host except the container engine itself.
#
# The Rust version must match rust-toolchain.toml exactly. The toolchain
# directory is read-only for the `dev` user, so a mismatch fails loudly instead
# of silently downloading another toolchain.
#
# Bun and Node are the runtimes of the runner contract tests. Their versions
# are recorded in compatibility.toml; change both places together. The npm
# packages of the contract fixture (Vitest, Storybook, Playwright) are pinned
# in tests/fixtures/runners/{package.json,bun.lock}.
ARG RUST_VERSION=1.100.0
ARG DEBIAN_CODENAME=trixie
ARG BUN_VERSION=1.4.4
ARG NODE_VERSION=26.13.1

FROM oven/bun:${BUN_VERSION} AS bun
FROM node:${NODE_VERSION}-${DEBIAN_CODENAME}-slim AS node

FROM rust:${RUST_VERSION}-slim-${DEBIAN_CODENAME} AS dev

ARG DEV_UID=1000
ARG DEV_GID=1000

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
COPY --from=node /usr/local/bin/node /usr/local/bin/node

RUN groupadd --gid "${DEV_GID}" dev \
 && useradd --uid "${DEV_UID}" --gid "${DEV_GID}" --create-home --shell /bin/bash dev \
 && mkdir -p /home/dev/.cargo /home/dev/.local/state /var/cache/testry/target /workspace /opt/testry/fixtures/runners \
 && chown -R dev:dev /home/dev/.cargo /home/dev/.local /var/cache/testry /workspace /opt/testry

# cargo/rustc proxies stay in /usr/local/cargo/bin (already on PATH); only the
# writable cargo state (registry, git checkouts, locks) moves to the dev user.
ENV CARGO_HOME=/home/dev/.cargo \
    CARGO_TARGET_DIR=/var/cache/testry/target \
    TESTRY_RUNNER_FIXTURE_MODULES=/opt/testry/fixtures/runners/node_modules

USER dev

# The bind-mounted workspace can appear with a different owner than `dev`
# (virtiofs maps ownership per accessing uid).
RUN git config --global --add safe.directory /workspace

# Dependencies of the runner contract fixture, installed from the lockfile at
# image build time. The gates run without network, so nothing is fetched then.
COPY --chown=dev:dev tests/fixtures/runners/package.json tests/fixtures/runners/bun.lock /opt/testry/fixtures/runners/
RUN cd /opt/testry/fixtures/runners && bun install --frozen-lockfile

# Chromium (headless shell) for the Playwright and Storybook (Vitest browser
# mode) contract tests. Its system libraries need root. The browser itself
# goes to the default location in the home of `dev`, so runners find it
# without any extra environment variable.
USER root
RUN node /opt/testry/fixtures/runners/node_modules/playwright/cli.js install-deps chromium \
 && rm -rf /var/lib/apt/lists/*
USER dev
RUN node /opt/testry/fixtures/runners/node_modules/playwright/cli.js install --only-shell chromium

WORKDIR /workspace
CMD ["bash"]
