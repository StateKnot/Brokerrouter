# Build context contains only the CI console package.json and package-lock.json.
# This is an isolated test-tool image, never a production workload image.
FROM node@sha256:a9f5f7c91a432850b2a8a7797adf5eadb6c733ceed61167806cee7ea7fbc29df
WORKDIR /workspace
COPY package.json package-lock.json ./
RUN npm ci --ignore-scripts --no-audit --no-fund
# Preserve official Debian archive signatures and TLS verification while bounding
# network retries for the browser dependency installer invoked by Playwright.
# Bootstrap the system CA package over verified HTTPS using official Node roots.
RUN printf 'Acquire::ForceIPv4 "true";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries "3";\nAPT::Update::Error-Mode "any";\n' > /etc/apt/apt.conf.d/80brokerrouter-verification-network && \
    sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources && \
    node -e 'require("node:fs").writeFileSync("/tmp/node-roots.pem", require("node:tls").rootCertificates.join("\n") + "\n")' && \
    apt-get -o Acquire::https::CaInfo=/tmp/node-roots.pem update && \
    apt-get -o Acquire::https::CaInfo=/tmp/node-roots.pem install --yes --no-install-recommends ca-certificates && \
    rm /tmp/node-roots.pem && \
    npx --no-install playwright install --with-deps chromium && \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=2 -o APT::Update::Error-Mode=any update && \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=2 install --yes --no-install-recommends libnss3-tools && \
    rm -rf /var/lib/apt/lists/*
ENV NODE_PATH=/workspace/node_modules
