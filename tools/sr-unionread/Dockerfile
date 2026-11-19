# syntax=docker/dockerfile:1.7
ARG RUST_IMAGE=rust:1.95.0-bookworm
ARG SR_BUILD_IMAGE=starrocks/dev-env-ubuntu:main-latest
FROM ${RUST_IMAGE} AS reader
WORKDIR /src/fluss-rust
COPY --from=fluss_src Cargo.toml Cargo.lock LICENSE NOTICE README.md ./
COPY --from=fluss_src crates ./crates
COPY --from=fluss_src bindings ./bindings
ENV CARGO_TARGET_DIR=/src/fluss-rust/target
RUN cargo build --locked --release -p fluss-lake-ffi --lib --bin seed && \
    nm -D target/release/libfluss_lake_ffi.so | grep -w fluss_union_client_create_sasl

FROM ${SR_BUILD_IMAGE} AS srbuild
USER root
WORKDIR /workspace
COPY --from=sr_src . /workspace/
COPY --from=reader /src/fluss-rust/bindings/lake/include /opt/fluss-include
ENV FLUSS_UNION_INCLUDE_DIR=/opt/fluss-include STARROCKS_HOME=/workspace STARROCKS_THIRDPARTY=/var/local/thirdparty STARROCKS_VERSION=ecs-unionread STARROCKS_COMMIT_HASH=b47765ca9e
ARG JOBS=4
RUN bash build.sh --fe --be -j "$JOBS" && test -x output/be/lib/starrocks_be && test -f output/fe/lib/starrocks-fe.jar && \
    test -s output/be/lib/fluss-reader-lib/starrocks-fluss-reader.jar

FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64
RUN apt-get update && apt-get install -y --no-install-recommends openjdk-21-jdk-headless libssl3t64 ca-certificates curl procps iproute2 net-tools tzdata python3 python3-pymysql tini && rm -rf /var/lib/apt/lists/*
COPY --from=srbuild /workspace/output/fe /opt/sr/fe
COPY --from=srbuild /workspace/output/be /opt/sr/be
COPY --from=reader /src/fluss-rust/target/release/libfluss_lake_ffi.so /opt/fluss/libfluss_lake_ffi.so
COPY --from=reader /src/fluss-rust/target/release/seed /opt/seed
COPY probe.py peak.py /opt/
ENV FLUSS_UNION_LIBRARY=/opt/fluss/libfluss_lake_ffi.so
RUN python3 -c 'import ctypes,os; getattr(ctypes.CDLL(os.environ["FLUSS_UNION_LIBRARY"]), "fluss_union_bucket_open_with_baseline_cancellable")' && \
    mkdir -p /opt/sr/fe/meta /opt/sr/be/storage && \
    printf '\nmem_limit=4G\npipeline_exec_thread_pool_thread_num=4\n' >> /opt/sr/be/conf/be.conf
ENTRYPOINT ["/usr/bin/tini","--"]
