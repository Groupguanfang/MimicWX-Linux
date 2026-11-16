FROM ubuntu:22.04 AS builder

ARG RUST_TOOLCHAIN=1.94.0
ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_BUILD_JOBS=2

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential ca-certificates curl perl pkg-config \
    libatspi2.0-dev libdbus-1-dev libglib2.0-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}" --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,id=mimicwx-cargo-registry,target=/root/.cargo/registry \
    --mount=type=cache,id=mimicwx-cargo-target,target=/build/target \
    cargo clean --release -p mimicwx \
    && cargo test --locked --release \
    && cargo build --locked --release \
    && cp target/release/mimicwx /build/mimicwx

FROM ubuntu:22.04

ARG WECHAT_VERSION=4.1.13.26
ARG WECHAT_SHA256=a0db8caeee8d1e6cfe0aa5ce58710270f8d74a90ff4920f53a3fc22aa0956fe9
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=zh_CN.UTF-8
ENV LANGUAGE=zh_CN:zh
ENV LC_ALL=zh_CN.UTF-8
ENV TZ=Asia/Shanghai

COPY lazycat/downloads/wechat-${WECHAT_VERSION}-amd64.deb /tmp/wechat.deb
RUN printf '%s  /tmp/wechat.deb\n' "${WECHAT_SHA256}" | sha256sum --check - \
    && apt-get update && apt-get install -y --no-install-recommends \
    /tmp/wechat.deb at-spi2-core ca-certificates curl dbus-x11 \
    fonts-noto-cjk fonts-wqy-microhei gdb libatomic1 libcap2-bin \
    libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 libcairo2 \
    libcups2 libdrm2 libgbm1 libgtk-3-0 libnss3 libpango-1.0-0 \
    libxcb-cursor0 libxcb-icccm4 libxcb-image0 libxcb-keysyms1 \
    libxcb-randr0 libxcb-render-util0 libxcb-shape0 libxcb-xinerama0 \
    libxcb-xkb1 libxkbcommon-x11-0 locales novnc openbox openssl \
    procps python3 tigervnc-common tigervnc-standalone-server tzdata \
    websockify x11-utils x11-xserver-utils xclip xterm \
    && test "$(dpkg-query -W -f='${Version}' wechat)" = "${WECHAT_VERSION}" \
    && locale-gen zh_CN.UTF-8 \
    && ln -snf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime \
    && useradd --create-home --uid 1000 --shell /bin/bash wechat \
    && rm -f /tmp/wechat.deb \
    && rm -rf /var/lib/apt/lists/*

RUN apt-get update && apt-get install -y --no-install-recommends \
    libegl1 libgl1 libegl-mesa0 libpulse0 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/mimicwx /usr/local/bin/mimicwx
COPY docker/dbus-mimicwx.conf /etc/dbus-1/session.d/mimicwx.conf
COPY lazycat/extract-key.py /usr/local/bin/extract-key.py
COPY lazycat/profile.sh /usr/local/bin/mimicwx-profile
COPY lazycat/entrypoint.sh /usr/local/bin/mimicwx-entrypoint
COPY lazycat/session.sh /usr/local/bin/mimicwx-session
COPY lazycat/index.html /usr/share/novnc/index.html
RUN chmod 0755 /usr/local/bin/mimicwx /usr/local/bin/mimicwx-entrypoint \
    /usr/local/bin/mimicwx-session /usr/local/bin/mimicwx-profile \
    && setcap cap_sys_admin+ep /usr/local/bin/mimicwx \
    && install -d -o wechat -g wechat -m 0700 \
    /home/wechat/.xwechat /home/wechat/mimicwx-linux /var/lib/mimicwx

WORKDIR /home/wechat/mimicwx-linux
EXPOSE 6080 8899
CMD ["/usr/local/bin/mimicwx-entrypoint"]
