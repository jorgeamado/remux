FROM mcr.microsoft.com/devcontainers/rust:1-1-bookworm

# Beyond the basics: chromium (Playwright e2e) runtime libs and fonts so
# UI screenshots render every glyph.
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
        tmux \
        zstd \
        jq \
        libnss3 libnspr4 libasound2 libatk1.0-0 libatk-bridge2.0-0 \
        libcups2 libdrm2 libxkbcommon0 libatspi2.0-0 libx11-6 \
        libxcomposite1 libxdamage1 libxext6 libxfixes3 libxrandr2 \
        libgbm1 libcairo2 libpango-1.0-0 libx11-xcb1 libxcursor1 \
        libgtk-3-0 fonts-liberation fonts-dejavu-core fonts-noto-core \
    && apt-get clean && rm -rf /var/lib/apt/lists/*
