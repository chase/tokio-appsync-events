# SPDX-FileCopyrightText: 2025 Chase Colman
# SPDX-License-Identifier: MPL-2.0

FROM rust:slim AS builder
WORKDIR /app

RUN cargo install cargo-chef
COPY Cargo.toml Cargo.lock ./

RUN cargo chef prepare --recipe-path recipe.json
RUN mkdir -p target/release
RUN cargo chef cook --features server --release --recipe-path recipe.json

COPY src ./src
RUN cargo build --release --features server --bin mock-appsync-server

FROM debian:12-slim
COPY --from=builder /app/target/release/mock-appsync-server /mock-appsync-server
ENV IN_DOCKER=true
EXPOSE 8080
ENTRYPOINT ["/mock-appsync-server"]
