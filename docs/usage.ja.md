# Usage Guide

[English](usage.md) | 日本語

日常的な導入手順は [README](../README.ja.md) を参照する。
ここでは設定、監視、リモート実行の使い方を扱う。

## Configuration (`config.toml`)

設定はすべて任意。
`$BUILDQ_DIR/config.toml` に配置する（標準パスは `buildq config` で確認可能）。

```toml
slots = 2                # ビルドの最大同時実行数 (デフォルト: 2)
# max_per_worktree = 1   # クラスごとの worktree 上限（デフォルト: ceil(slots / 2)、0 で無効）
run_slots = 2            # `cargo run` のアプリ最大同時実行数 (デフォルト: 2)

[cache]
max_members = 3          # 最大 target/ 数 (最小: slots、デフォルト: slots + 1)
budget = "45G"           # BUILDQ_CACHE 全体の最大ディスク使用量 (デフォルト: "60G")
min_free = "25G"         # ドライブ上に最低限残す空き容量 (デフォルト: "20G")
# pool_explicit = true   # worktree 内を指す --target-dir / CARGO_TARGET_DIR もプールする (デフォルト: false)

[cargo]
rustc_wrapper = "none"   # ジョブごとにグローバルな sccache 等を無効化 ("none" | "keep" | "/path")
# jobs = 8               # 1ビルドあたりの CARGO_BUILD_JOBS (デフォルト: コア数 / slots)
```

`cache.max_members` の最小値は、環境変数の上書きを含む有効な `slots` の値である。
それより小さい値を指定した場合は `slots` まで引き上げ、指定値・実際の上限・修正方法を警告として表示する。
プロジェクトごとの上限を `slots` より小さく保つ構成には、現時点では対応していない。

| 設定 | 環境変数による上書き | デフォルト |
| :--- | :--- | :--- |
| `max_per_worktree` | `BUILDQ_MAX_PER_WORKTREE` | `ceil(slots / 2)`、`0` で無効 |

プール内のビルドが `min_free` の診断（終了コード `125`）で失敗した場合は、
キャッシュのボリュームの空き容量を回復して再試行する。
開始前には GC を1回試み、実行中も空き容量の基準を下回れば停止する。
`buildq cache ls --refresh` と `buildq gc` で未使用キャッシュを確認・回収できる。
ボリュームに適した値を設定する。`min_free = "0"` で保護を無効化できる。
対象範囲と制限は [Free-Space Guard](architecture.ja.md#free-space-guard) を参照する。

## Live Monitoring

`buildq top` は実行中ジョブ、待機列と待機時間、リモートの待機列、ターゲットプール容量を1秒ごとに更新する。
`--interval N` で更新間隔（秒）、`--no-hosts` でローカルのみの表示を指定できる。
リモート状態はバックグラウンドで取得するため、応答の遅いホストがあってもローカル表示を継続する。
リモートの問い合わせは最短5秒間隔で行い、問い合わせごとに5秒の制限時間を設ける。
終了は Ctrl+C、全行のスナップショット表示は `--once` を使用する。
出力をリダイレクトした場合も、制御文字を含まない1回のスナップショットになる。
プール容量は `cache ls` と同じ概算値である。

## Remote Builds (SSH)

`config.toml` にホストを追記すると、SSH と Git 経由でリモートマシンへビルドを分散できる。

```toml
auto_hosts = ["local", "box"]

[hosts.box]
# transport = "rsync"    # gitignore 対象の生成ファイルも同期（任意）
ssh = "box"              # ~/.ssh/config のホスト名
slots = 4                # このホストへ同時に送る最大ジョブ数
# buildq = "buildq"      # 自動選択前にリモートの待機列を確認
# rustc_release = "1.99.0" # リモートの Rust リリースを検査（任意）
```

ホスト名を指定するか、`auto` で空きホストを自動選択して実行する。

```sh
buildq run --host box -- cargo test
buildq run --host auto -- cargo clippy --all-targets
buildq run --host box --fetch target/debug/app -- cargo build --bin app
```

ホスト選択の判定・同期方式の詳細は [Remote Execution Protocol](architecture.ja.md#5-remote-execution-protocol-ssh) を参照する。
