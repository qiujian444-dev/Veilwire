# 中继镜像
#
# 构建：docker build -f Dockerfile -t anonchat-relay .
# 运行（只做 WS 中继）：docker run -d -p 127.0.0.1:8080:8080 -v relay-data:/data anonchat-relay
# 运行（自带 TURN）：在镜像名后追加参数，注意要额外映射 UDP 端口
#   docker run -d --network host -v relay-data:/data anonchat-relay \
#     --turn 0.0.0.0:3478 --turn-external-ip <公网IP> \
#     --turn-user <用户> --turn-pass <密码> \
#     --turn-port-min 49160 --turn-port-max 49200
#
# 注意：容器里只监听 127.0.0.1 是没用的，所以容器内绑 0.0.0.0，
# 靠 docker 的 -p 127.0.0.1:8080:8080 把暴露面限制在本机。

FROM rust:1-slim AS build
WORKDIR /src
# 先只拷 manifest，利用层缓存避免每次改代码都重下依赖
COPY Cargo.toml Cargo.lock ./
COPY crates/proto/Cargo.toml crates/proto/
COPY crates/identity/Cargo.toml crates/identity/
COPY crates/secure/Cargo.toml crates/secure/
COPY crates/relay/Cargo.toml crates/relay/
COPY crates/punch/Cargo.toml crates/punch/
COPY crates/cli/Cargo.toml crates/cli/
RUN mkdir -p crates/proto/src crates/identity/src crates/secure/src \
             crates/relay/src crates/punch/src crates/cli/src \
 && echo "" > crates/proto/src/lib.rs \
 && echo "" > crates/identity/src/lib.rs \
 && echo "" > crates/secure/src/lib.rs \
 && echo "fn main(){}" > crates/relay/src/main.rs \
 && echo "fn main(){}" > crates/punch/src/main.rs \
 && echo "fn main(){}" > crates/cli/src/main.rs \
 && cargo build --release -p relay || true

COPY crates ./crates
RUN cargo build --release -p relay

FROM debian:bookworm-slim
RUN useradd -r -s /usr/sbin/nologin relay \
 && mkdir -p /data && chown relay:relay /data
COPY --from=build /src/target/release/relay /usr/local/bin/relay
USER relay
VOLUME ["/data"]
EXPOSE 8080
# TURN：3478/udp 是控制口，49160-49200/udp 是媒体中继口（与 --turn-port-* 保持一致）
EXPOSE 3478/udp
EXPOSE 49160-49200/udp
ENTRYPOINT ["relay", "serve", "--addr", "0.0.0.0:8080", "--data", "/data"]
