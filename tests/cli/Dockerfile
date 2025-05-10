FROM ubuntu:20.04

RUN apt-get -y update && \
    apt-get -y install libssl-dev openssl && \
    apt-get clean && \
    apt-get autoremove

# Create a non-root user and group
RUN groupadd -r cratetorrent && useradd -r -g cratetorrent cratetorrent

WORKDIR /cratetorrent

COPY /target/release/test-cli .

# Set proper ownership
RUN chown -R cratetorrent:cratetorrent /cratetorrent

# Switch to non-root user
USER cratetorrent

CMD ./test-cli --listen ${LISTEN} --mode ${MODE} --seeds ${SEEDS} --metainfo ${METAINFO_PATH} --download-dir ${DOWNLOAD_DIR}
