FROM rust:1.49 as builder

RUN USER=root cargo new --bin pod-watcher
WORKDIR /pod-watcher
COPY ./Cargo.toml ./Cargo.toml
COPY ./Cargo.lock ./Cargo.lock
RUN cargo build --release
RUN rm src/*.rs
RUN rm ./target/release/deps/pod_watcher*

ADD . ./

RUN cargo build --release

# Verify that the CLI is accessable
RUN /pod-watcher/target/release/pod-watcher --help

FROM debian:buster-slim as tmp

ENV TINI_VERSION v0.18.0

ADD https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini /tini
RUN chmod +x /tini

FROM debian:buster-slim
RUN apt-get update && apt-get install -y openssl && apt-get clean
ARG APP=/app

ENV TZ=Etc/UTC \
    APP_USER=appuser

RUN groupadd $APP_USER \
    && useradd -g $APP_USER $APP_USER \
    && mkdir -p ${APP}

COPY --from=tmp /tini /tini
COPY --from=builder /pod-watcher/target/release/pod-watcher ${APP}/pod-watcher

RUN chown -R $APP_USER:$APP_USER ${APP}

USER $APP_USER
WORKDIR ${APP}

ENTRYPOINT ["/tini", "--"]
CMD [ "/app/pod-watcher"]