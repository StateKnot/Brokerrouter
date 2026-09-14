# Build in a dedicated release pipeline; BASE_IMAGE must be an approved Debian digest.
# No signature updates, credentials or downloads occur when this image runs.
ARG BASE_IMAGE
FROM ${BASE_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends clamav ffmpeg \
    && rm -rf /var/lib/apt/lists/* /var/lib/clamav/*
COPY signatures/ /opt/clamav/
RUN chmod -R a+rX /opt/clamav && test -n "$(ls -A /opt/clamav)"
USER 65532:65532
WORKDIR /tmp
ENTRYPOINT ["/usr/bin/clamscan"]
