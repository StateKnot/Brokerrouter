# syntax=docker/dockerfile:1.7
# Official PostgreSQL 17.11 image: its signed Debian repositories supply Python.
FROM postgres@sha256:67f41722b7a8cbdb868a44a4995c846eddfdc2973bccb291ce937dce88ad5675
RUN apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 \
      -o Acquire::Retries=2 -o APT::Update::Error-Mode=any update && \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 \
      -o Acquire::Retries=2 install --yes --no-install-recommends python3 && \
    rm -rf /var/lib/apt/lists/*
ENV PYTHONDONTWRITEBYTECODE=1
COPY deploy/production/purpose-logins.py /opt/brokerrouter/deploy/production/purpose-logins.py
ENTRYPOINT ["python3", "/opt/brokerrouter/deploy/production/purpose-logins.py", "apply", "/run/purpose-secrets", "/run/secrets/purpose_admin_service", "purpose"]
