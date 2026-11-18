FROM rust:bookworm AS build
WORKDIR /src
COPY . .
RUN if [ -f Cargo.lock ]; then cargo build --locked --release -p rusty-balancer; else cargo build --release -p rusty-balancer; fi

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin balancer
WORKDIR /app
COPY --from=build /src/target/release/rusty-balancer /usr/local/bin/rusty-balancer
COPY configs/container.toml /app/configs/container.toml
COPY examples/public /app/examples/public
USER 10001
EXPOSE 8080
ENTRYPOINT ["rusty-balancer"]
CMD ["--config", "configs/container.toml", "serve"]
