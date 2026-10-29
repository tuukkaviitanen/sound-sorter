FROM jdxcode/mise:2026.10.23-debian AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV CARGO_BUILD_JOBS=2
COPY mise.toml ./
RUN mise trust && mise install rust
COPY sorter/Cargo.toml sorter/Cargo.lock ./sorter/
COPY sorter/src ./sorter/src
COPY sorter/tests ./sorter/tests
RUN mise run test && mise run build

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/sorter/target/release/upload-sorter /usr/local/bin/upload-sorter
ENTRYPOINT ["/usr/local/bin/upload-sorter"]
