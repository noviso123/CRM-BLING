FROM rust:1.85-slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home app
WORKDIR /app
COPY --from=build /app/target/release/crm-bling /usr/local/bin/crm-bling
RUN mkdir -p /data && chown app:app /data
USER app
ENV DATABASE_PATH=/data/whatsapp.sqlite
ENV COOKIE_SECURE=true
ENV PORT=3000
VOLUME ["/data"]
EXPOSE 3000
CMD ["crm-bling"]
