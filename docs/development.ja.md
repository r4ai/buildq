# Development & Setup Guide

[English](development.md) | 日本語

本ガイドでは `buildq` のディレクトリ構成、テストの実行手順、マルチエージェント・サンドボックス環境の設定手順を解説する。

## Repository Structure

```text
src/
  main.rs      # CLI エントリポイント・サブコマンドのディスパッチ
  lib.rs       # ライブラリクレート定義
  config.rs    # 設定読み込み (デフォルト < config.toml < 環境変数)
  shim.rs      # cargo shim のインストールと起動判定
  job.rs       # ジョブ実行制御 (キュー取得・プールリース・2フェーズ cargo run)
  queue.rs     # OS ファイルロックを用いたクラス別優先度付き FIFO キュー
  pool.rs      # target/ プール管理とメンバー選択スコアリング
  git.rs       # Git blob マニフェスト計算と親なしスナップショット作成
  cargo.rs     # Cargo 引数パース・エイリアス解決・メタデータ取得
  gc.rs        # ディスク予算管理と5段階ガベージコレクション
  remote.rs    # SSH + Git リモート実行トランスポートとプロトコル
  exec.rs      # プロセス起動・シグナル転送・Windows Job Object 制御・Unix 監視プロセス
  lock.rs      # クロスプラットフォーム OS ファイルロック (LockFileEx / flock)
  history.rs   # ジョブ実行履歴の記録と読み出し
  external.rs  # プール外で使われた明示 target ディレクトリの記録
  util.rs      # バイト表記・時間フォーマット・パス正規化ユーティリティ
tests/
  cli.rs       # 結合テスト (並列数制限・優先度・shim・GC・リモートプロトコル)
  e2e.rs       # 2つの Git worktree を用いた実 Cargo ビルド・無効化ベンチマーク
  fixtures/    # E2E テスト用のサンプルワークスペース
```

## Running Tests

### Unit & Integration Tests

単体テスト、複数プロセスによるキュー制御テスト、shim ルーティング、GC、ローカル擬似トランスポートによるリモートプロトコルテストを実行する。

```sh
cargo test
```

検証項目：
- 実プロセス間での同時実行数が `slots` を絶対に超えないこと
- 同一優先度内での FIFO 順序保証、および高優先度ジョブによる待機ジョブの追い越し
- スロット保持プロセスの強制終了時にスロットが即座に解放されること
- ネスト呼び出し（`BUILDQ_HELD`）がデッドロックせずキューをバイパスすること
- 終了コード（非ゼロや Windows NTSTATUS を含む）が正確に透過されること
- `cargo` shim が重いコマンド（エイリアス含む）をキューに入れ、軽いコマンドを素通しすること
- メンバー選択・マニフェスト無効化・予算超過時の GC が使用中メンバーを保護すること
- 2フェーズ `cargo run` のバイナリ / DLL 差分同期と JSON 成果物パース
- リモートスナップショット作成・差分プッシュ・worktree チェックアウト・実行・`--fetch` の一連の動作

### End-to-End (E2E) Cargo Tests

`tests/fixtures/sample-ws` を用い、2つの Git worktree 上で実際の `cargo` ビルドを実行する（ローカルの Cargo キャッシュに `regex` クレートが必要）。

```sh
cargo test --test e2e -- --ignored --nocapture
```

### Real SSH Integration Tests


Linux の SSH 実機テストには、パスワードなしで接続できる既存ホストを使う。
非対話シェルの PATH に Git、Cargo/Rust、scp、buildq があり、ホスト鍵が信頼済みであることが前提である。
テスト専用の一時リポジトリだけを送り、リモートの `/tmp` 配下に専用ディレクトリを作成・削除する。
ビルドにはホストの通常の buildq キューとプールを使う。SSH 設定は変更しない。

```sh
ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=5 localhost true
BUILDQ_SSH_TEST_HOST=localhost cargo test --test remote_ssh -- --nocapture
```

localhost での実行は、最初のコマンドが既存の設定で成功する場合に限る。
リモートの buildq に絶対パスが必要なら `BUILDQ_SSH_TEST_BUILDQ` を設定する。
`BUILDQ_SSH_TEST_HOST` 未設定時はスキップを報告し、接続しない。
未コミットのソース同期、2回の増分 Cargo ビルド、成果物取得、終了コード、
`status --hosts`、実際の PTY 経由の Ctrl+C を検証する。
依存のない fixture をオフラインでビルドするため、既定の Rust toolchain が導入済みである必要がある。


## Python Benchmark Tools

`scripts/` は uv で管理する独立した Python プロジェクトである。
Python 3.12.14 を `.python-version`、依存バージョンと配布物のハッシュを `uv.lock` で固定する。
以下はリポジトリルートで実行する。

```sh
uv sync --project scripts --locked
uv run --project scripts --locked python scripts/stress-bevy.py \
  --root /tmp/buildq-bevy-new \
  --buildq "$PWD/target/release/buildq"
uv run --project scripts --locked python scripts/plot-bevy-benchmark.py
```

- 測定前に Rust 環境を有効化し、`cargo build --release` で buildq を作成
- 測定先は新規ディレクトリ
- Linux で約 12 GiB 以上の空き容量が必要
- 日本語の図はシステムの Noto Sans CJK を使用
  - 別の配置先は `--font /path/to/NotoSansCJK-Regular.ttc` で指定
- PNG プレビューは `--preview-dir /tmp/buildq-figures` で任意に出力

各 CLI は引数処理のみを持ち、実装は通常の import で共有する。

| モジュール | 役割 |
| :--- | :--- |
| `buildq_bench/measurement.py` | サブプロセスの測定、リソース集計、JSONL 出力 |
| `buildq_bench/scenarios.py` | worktree・キャッシュ・並行実行の検証シナリオ |
| `buildq_bench/plotting.py` | 集計データから日英の比較図を生成 |

```sh
uv run --directory scripts --locked pytest
uv run --directory scripts --locked ruff check .
uv run --directory scripts --locked ruff format --check .
```

依存を変更した場合は `uv lock --project scripts` を実行し、`pyproject.toml` と `uv.lock` を同時にコミットする。
`.venv`・ツールキャッシュ・`__pycache__` はコミットしない。

## Multi-Agent Sandbox Recipe

同一マシン上で複数のコーディングエージェント（Codex CLI、Claude Code 等）を制限付きサンドボックス内で動かす場合、全エージェントが `$BUILDQ_DIR` と `$BUILDQ_CACHE` に読み書きできる必要がある。

### 1. Create a Short Shared Directory

worktree と同一ドライブ上に、**パスが短いディレクトリ**を用意する（`target/` 内のパスが深いため、Windows の `MAX_PATH` 制限を回避するため）。

- **Windows**: `C:\bq`
- **Unix / macOS**: `/tmp/bq` または `/var/tmp/bq`

### 2. Configure Sandbox Permissions

- **エージェントの書き込み許可リスト**
  - 各エージェントの writable roots 設定に `C:\bq`（または `/tmp/bq`）を追加
- **Windows ACL（アクセス制御リスト）**
  - サンドボックスは制限付きトークンで動作する
  - 別エージェントが作成したファイルを更新できるよう、継承可能な変更権限（Modify）を付与
    ```powershell
    icacls C:\bq /grant "<sandbox-user-or-group>:(OI)(CI)M"
    ```

### 3. Set Agent Environment Variables

ユーザー環境変数またはエージェントの起動スクリプトに以下を設定する。

**PowerShell (Windows)**
```powershell
$env:BUILDQ_DIR   = "C:\bq"
$env:BUILDQ_CACHE = "C:\bq\c"
$env:PATH         = "C:\bq\bin;$env:PATH"
```

**POSIX Shell (macOS / Linux / Git Bash)**
```sh
export BUILDQ_DIR=/tmp/bq
export BUILDQ_CACHE=/tmp/bq/c
export PATH="/tmp/bq/bin:$PATH"
```

### 4. Install the Shim

```sh
buildq shim install cargo --into C:\bq\bin
```

- **Windows**
  - `buildq.exe` を `cargo.exe` としてコピーし、`.buildq-shim` マーカーを配置
  - PowerShell・`cmd.exe`・Git Bash のすべてで同一に解決される
  - `buildq` 本体をアップグレードした際は `buildq shim install` を再実行する
- **Unix**
  - 2行のシェルスクリプトを生成（Windows でも `--sh` で生成可能）

## Environment Variables Reference

| 変数名 | 説明 |
| :--- | :--- |
| `BUILDQ_DIR` | キューのロックファイル・`config.toml`・履歴の保存先ディレクトリ |
| `BUILDQ_CACHE` | ターゲットプール（`pools/`）および実行ディレクトリ（`runs/`）のルート |
| `BUILDQ_CONFIG` | 設定ファイルの明示的なパス<br>（デフォルト: `$BUILDQ_DIR/config.toml`） |
| `BUILDQ_SLOTS` | `slots`（`build` クラスの最大同時実行数）を上書き |
| `BUILDQ_BUDGET` | ディスク予算を上書き（例: `45G`） |
| `BUILDQ_POOL` | `0` を指定するとターゲットプールを無効化（キューのみ使用） |
| `BUILDQ_POOL_EXPLICIT` | `1` を指定すると worktree 内の明示 target ディレクトリもプールする<br>（`[cache] pool_explicit` と同じ） |
| `BUILDQ_GC` | `0` を指定するとジョブ終了後の自動バックグラウンド GC を無効化 |
| `BUILDQ_PRIORITY` | `cargo` shim 経由での実行時のデフォルト優先度 |
| `BUILDQ_LABEL` | `status` や `history` に表示する任意のジョブラベル |
| `BUILDQ_REAL_CARGO` | 本来の `cargo` バイナリのパスを明示指定 |
| `BUILDQ_QUIET` | `1` を指定すると `buildq:` のステータス出力を抑制 |
| `BUILDQ_TRACE` | `1` を指定すると各フェーズの所要時間を詳細出力 |

## Releases

Rust edition は 2024、MSRV は 1.89 である。CI とリリースには
`rust-toolchain.toml` の Rust 1.99.0 を使用する。
ツールチェーン更新時は両バージョンの CI 成功を確認する。
CI は整形、厳格な Clippy、ドキュメント、依存関係、および Linux・macOS・Windows
での実 Cargo E2E を検査する。Dependabot は週次・グループ単位で更新を提案し、
7 日間のクールダウンを設ける。セキュリティ更新は速やかにレビューする。

1. `Cargo.toml` の `package.version` を更新し、`cargo update` で `Cargo.lock` を更新する。
   レビューと CI 成功後、`main` にマージする
2. 対象コミットに `git tag -a v0.1.0 -m "Release v0.1.0"` でタグを付け、
   `git push origin v0.1.0` でタグを送信する。バージョンは適宜置き換える
3. タグ用ワークフローが CI、バージョン一致、`main` の履歴への所属を再検査する。
   Linux・macOS の x86_64/aarch64 と Windows x86_64 向けバイナリをビルドし、
   アーカイブ、ライセンス、`SHA256SUMS`、GitHub 自動生成のリリースノートを公開する

`v0.2.0-rc.1` などのタグは `Cargo.toml` と同じバージョンを必要とし、プレリリースを作成する。
各アーカイブの来歴は `gh attestation verify ARCHIVE --repo r4ai/buildq` で検証できる。
ダウンロード後は `sha256sum -c SHA256SUMS`、macOS では
`shasum -a 256 -c SHA256SUMS` でチェックサムを確認する。
全アセットのアップロード後にドラフトを公開する。
公開処理がドラフト作成後に失敗した場合、ドラフトを確認・削除してから失敗ジョブを再実行する。
公開済みタグは移動しない。

管理者は Actions と artifact attestations、Dependabot security updates、
非公開の脆弱性報告を有効にし、`main` の ruleset で **CI passed** を必須にする。
`v*` タグの作成・更新・削除はリリース担当者に制限する。
公開リポジトリでは `GITHUB_TOKEN` と GitHub OIDC のみを使用し、PAT や署名用秘密鍵は不要である。
[Security policy](../SECURITY.md) も参照する。
