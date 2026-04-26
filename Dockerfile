FROM rust:1.73-alpine AS builder

RUN apk add --no-cache musl-dev
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs \
    && cargo build --release \
    && strip target/release/cpanel-nas-backup

FROM scratch

COPY --from=builder /app/target/release/cpanel-nas-backup /cpanel-nas-backup
VOLUME ["/backups"]
ENTRYPOINT ["/cpanel-nas-backup"]
