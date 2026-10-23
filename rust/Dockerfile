# 中继镜像
#
# 构建（在 rust/ 目录下）：
#   docker build -t veilwire-relay:0.15.0 .
#
# 只做 WS 中继：
#   docker run -d --name relay -p 127.0.0.1:8080:8080 -v relay-data:/data veilwire-relay:0.15.0
#
# 自带 TURN + 公布到 DHT（额外参数直接追加在镜像名后面）：
#   docker run -d --name relay --network host --restart unless-stopped \
#     -v /var/lib/relay:/data veilwire-relay:0.15.0 \
#     --turn 0.0.0.0:3478 --turn-external-ip <公网IP> \
#     --turn-user <用户> --turn-pass <密码> \
#     --turn-port-min 49160 --turn-port-max 49200 \
#     --dht-announce 8080
#
# 注意：容器里只监听 127.0.0.1 是没用的，所以容器内绑 0.0.0.0，
# 靠 docker 的 -p 127.0.0.1:8080:8080 把暴露面限制在本机。
#
# 构建用 cache mount 复用依赖缓存：改一行代码重建时不会重新下载、重编整个依赖树
# （2 核机器上差 10 分钟以上）。

FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p relay \
 && cp /src/target/release/relay /usr/local/bin/relay.bin

FROM debian:bookworm-slim
RUN useradd -r -s /usr/sbin/nologin relay \
 && mkdir -p /data && chown relay:relay /data
COPY --from=build /usr/local/bin/relay.bin /usr/local/bin/relay
USER relay
VOLUME ["/data"]
EXPOSE 8080/tcp
# TURN：3478/udp 是控制口，49160-49200/udp 是媒体中继口（与 --turn-port-* 保持一致）
EXPOSE 3478/udp
EXPOSE 49160-49200/udp
# ENTRYPOINT 带上默认的 serve/--addr/--data；额外的 flag 由 docker run 追加在后面。
ENTRYPOINT ["relay", "serve", "--addr", "0.0.0.0:8080", "--data", "/data"]
