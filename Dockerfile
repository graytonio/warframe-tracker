FROM node:25-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web ./
RUN npm run build

# Pinned to bookworm to match the runtime's glibc (plain rust:1.94 is trixie, glibc 2.41).
FROM rust:1.94-bookworm AS build
WORKDIR /app
# Build dependencies against stub sources so this layer stays cached until Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs && cargo build --release && rm -rf src
# src/db.rs include_str!s the schema.
COPY schema.sql ./
COPY src ./src
# touch: the real sources can be older than the stub's build output, which would make cargo skip them.
RUN touch src/main.rs src/lib.rs && cargo build --release

FROM debian:bookworm-slim
# The importer makes HTTPS calls to the wiki.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/warframe-tracker /usr/local/bin/warframe-tracker
COPY --from=web /web/dist /app/web
ENV WEB_DIR=/app/web
EXPOSE 3000
ENTRYPOINT ["warframe-tracker"]
CMD ["serve"]
