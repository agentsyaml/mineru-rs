FROM rust:1.92.0-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder

WORKDIR /app
RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*
COPY tools/deps/ tools/deps/
COPY . .
RUN python3 tools/deps/prepare_office.py prepare \
    && python3 tools/deps/prepare_office.py verify \
    && python3 -m unittest discover -s tools/deps -p 'test_*.py'
# Rust API image; VLM inference uses an external provider.
RUN cargo build --release --locked -p mineru --features office,legacy-office --bins

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system mineru \
    && useradd --system --gid mineru --home-dir /app --shell /usr/sbin/nologin mineru \
    && mkdir -p /app/output \
    && chown mineru:mineru /app/output

COPY --from=builder /app/target/release/mineru /usr/local/bin/mineru
COPY --from=builder /app/target/release/mineru-api /usr/local/bin/mineru-api
COPY --from=builder /app/target/release/mineru-office-convert /usr/local/bin/mineru-office-convert

WORKDIR /app
USER mineru
ENV MINERU_API_PUBLIC_BIND_EXPOSED=true \
    MINERU_API_OUTPUT_ROOT=/app/output
EXPOSE 8000
VOLUME ["/app/output"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD curl --fail --silent --show-error http://127.0.0.1:8000/health || exit 1
CMD ["mineru-api", "--host", "0.0.0.0", "--port", "8000"]
