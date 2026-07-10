FROM mcr.microsoft.com/devcontainers/rust:1-1-bookworm

RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
        tmux \
        zstd \
        jq \
    && apt-get clean && rm -rf /var/lib/apt/lists/*
