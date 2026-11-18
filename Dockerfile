FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rusty-balancer

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --create-home balancer
WORKDIR /app
COPY --from=build /src/target/release/rusty-balancer /usr/local/bin/rusty-balancer
COPY configs/balancer.toml /app/configs/balancer.toml
COPY examples/public /app/examples/public
USER 10001
EXPOSE 8080
ENTRYPOINT ["rusty-balancer"]
CMD ["serve"]
