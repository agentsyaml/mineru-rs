FROM rust:1.89.0-bookworm@sha256:948f9b08a66e7fe01b03a98ef1c7568292e07ec2e4fe90d88c07bb14563c84ff AS builder

WORKDIR /app
COPY . .
RUN cargo build --release --locked -p mineru --features office --bins

FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system mineru \
    && useradd --system --gid mineru --home-dir /app --shell /usr/sbin/nologin mineru \
    && mkdir -p /app/output \
    && chown mineru:mineru /app/output

COPY --from=builder /app/target/release/mineru /usr/local/bin/mineru
COPY --from=builder /app/target/release/mineru-vlm /usr/local/bin/mineru-vlm
COPY --from=builder /app/target/release/mineru-api /usr/local/bin/mineru-api
COPY --from=builder /app/target/release/mineru-vlm-api /usr/local/bin/mineru-vlm-api
COPY --from=builder /app/target/release/mineru-office-convert /usr/local/bin/mineru-office-convert

WORKDIR /app
USER mineru
ENV MINERU_API_PUBLIC_BIND_EXPOSED=true \
    MINERU_API_OUTPUT_ROOT=/app/output
EXPOSE 8000
VOLUME ["/app/output"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD curl --fail --silent --show-error http://127.0.0.1:8000/health || exit 1
CMD ["mineru-api", "--host", "0.0.0.0", "--port", "8000"]
