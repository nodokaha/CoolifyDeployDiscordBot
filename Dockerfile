# =========================================================================
# 1. ビルド用のステージ（Rustの公式イメージを使用）
# =========================================================================
FROM rust:slim-bookworm AS builder

WORKDIR /app

# 依存ライブラリのキャッシュを利用するためのハック
# (Cargo.tomlが変わらない限り、重いコンパイルをスキップできるようにします)
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

# 実際のソースコードをコピーして本番ビルド
COPY src ./src
# main.rsのタイムスタンプを更新して、確実に再ビルドさせる
RUN touch src/main.rs
RUN cargo build --release

# =========================================================================
# 2. 実行用のステージ（超極小・安全なディストロレスイメージを使用）
# =========================================================================
FROM gcr.io/distroless/cc-debian12 AS runner

WORKDIR /app

# ビルドステージからコンパイル済みのバイナリだけをコピー
COPY --from=builder /app/target/release/CoolifyDeployDiscordBot ./app-server

# コンテナ外部からアクセスできるように環境変数を設定（任意）
ENV RUST_LOG=info

# バイナリを直接実行
CMD ["./app-server"]
