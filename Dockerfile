FROM rust:1.47 as builder

RUN USER=root cargo new --bin slack-user-cache
WORKDIR /slack-user-cache
COPY ./Cargo.toml ./Cargo.toml
COPY ./Cargo.lock ./Cargo.lock
RUN cargo build --release
RUN rm src/*.rs

ADD . ./

RUN rm ./target/release/deps/slack_user_cache*
RUN cargo build --release

# Verify that the CLI is accessable
RUN /slack-user-cache/target/release/slack-user-cache web --help

FROM debian:buster-slim
ARG APP=/app

RUN apt-get update \
    && apt-get install -y tzdata \
    && rm -rf /var/lib/apt/lists/*

EXPOSE 3000

ENV TZ=Etc/UTC \
    APP_USER=appuser

ENV TINI_VERSION v0.18.0

ADD https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini /tini
RUN chmod +x /tini
RUN groupadd $APP_USER \
    && useradd -g $APP_USER $APP_USER \
    && mkdir -p ${APP}

COPY --from=builder /slack-user-cache/target/release/slack-user-cache ${APP}/slack-user-cache

RUN chown -R $APP_USER:$APP_USER ${APP}

USER $APP_USER
WORKDIR ${APP}

ENTRYPOINT ["/tini", "--"]
CMD [ "/app/slack-user-cache"]