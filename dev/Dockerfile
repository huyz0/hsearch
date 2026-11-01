FROM rust:1.97.1-bookworm@sha256:0e2bcaef56d041a486784e54104a81aebe0da44bd03019bd70bc0401e42e4a97

# The immutable base already supplies Python 3.11 and OpenSSL. No floating apt layer.
RUN rustup component add rustfmt clippy llvm-tools-preview \
    && python3 --version && openssl version \
    && mkdir -p /cargo /target /workspace /hsearch-state/ca /hsearch-state/server /hsearch-state/client /hsearch-state/credentials \
    && chown 10001:10001 /cargo /target /workspace /hsearch-state /hsearch-state/ca /hsearch-state/server /hsearch-state/client /hsearch-state/credentials

ENV CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target \
    CARGO_BUILD_JOBS=2 \
    CARGO_TERM_COLOR=never \
    PYTHONDONTWRITEBYTECODE=1
USER 10001:10001
WORKDIR /workspace
CMD ["python3", "dev/execute.py", "diagnostics", "--disk-bytes", "8589934592", "--timeout", "60"]
