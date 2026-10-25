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
# pool_fallback = true   # プールを開けない (Git やツールチェーンの失敗) とき、失敗せず警告してプール外でビルドする (デフォルト: false、環境変数 BUILDQ_POOL_FALLBACK)
# dedupe = "off"         # GC でメンバー間の同一ファイルを共有する ("auto" | "off"、デフォルト: "auto")
# seed_members = true    # 新しいメンバーを最も近いビルド済みメンバーの copy-on-write クローンから始め、worktree ごとに温かいメンバーを残す (APFS、Btrfs、XFS、実験的、デフォルト: false、環境変数 BUILDQ_SEED_MEMBERS)

[cargo]
rustc_wrapper = "none"   # ジョブごとにグローバルな sccache 等を無効化 ("none" | "keep" | "/path")
# jobs = 8               # 1ビルドあたりの CARGO_BUILD_JOBS (デフォルト: コア数 / slots)
# backend = "mbx"        # キューのジョブを mr-boxington 経由で実行 ("cargo" | "mbx"、デフォルト: "cargo")
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

## Automatic GC

ジョブ終了後の GC は `[cache] gc_mode = "background"` がデフォルトである。
Unix の切り離された GC 子プロセスには、引き取った子を回収する init が必要である。
コンテナでは Docker の `--init`、または PID 1 として `tini` / `dumb-init` を使用する。
子を回収しない PID 1 では終了済み GC がゾンビとして蓄積しうる。
これらは実行中のコンパイラではなく、スロットも保持しない。
`buildq config` は有効なモードと init の要件を表示する。

GC は同時に一つだけ実行される。
ほかの GC の実行中にジョブ後の GC が起動すると、要求を捨てずにキャッシュディレクトリへ `gc.pending` を残す。
実行中の GC はロックを解放した後、この要求に対して最大3回の追加パスを実行するため、そのパスの間ビジーだったメンバーもジョブ終了後に再検討される。
`gc --auto --json` はこれを `follow_ups` として報告する。

子を回収する init がない環境では、ジョブのプロセス内で回収する設定を選べる。

```toml
[cache]
gc_mode = "inline"
```

インライン GC はビルドスロットと解放可能なメンバーリースを解放した後に実行し、完了を待つ。
回収時間が終了までの待ち時間に加わる。自動削除の警告はビルドの終了コードを変更しない。
両モードとも、概算使用量が予算内なら10分以内の回収を省略する。
バックグラウンドモードも子プロセス作成前に判定し、省略するだけの GC は起動しない。

別の方法として、`BUILDQ_GC=0` を設定し、管理側から前景の `buildq gc` を呼んで終了を待てる。
手動 GC と空き容量の開始前チェックは引き続き利用できる。
[Container Process Lifecycle](development.ja.md#container-process-lifecycle) を参照する。

## Temporary Directory Isolation

プールメンバーとキュースロットは一時ファイルの fixture を分離しない。
固定の一時ルート内でプロセス内カウンターを使うテストは、独立した worktree 間でも衝突しうる。
これは buildq を使わない直接実行でも起こる。デフォルトでは呼び出し元の一時ディレクトリ環境を維持する。

呼び出しごとに新しいディレクトリを作り、コマンドと子孫へ3つの環境変数を渡すことで分離を選べる。
次の POSIX の例は、コマンドが返る時点でプロセスツリー全体が終了する場合を対象とする。

```sh
job_tmp="$(mktemp -d /var/tmp/buildq-job.XXXXXXXX)" || exit 1
job_status=0
TMPDIR="$job_tmp" TMP="$job_tmp" TEMP="$job_tmp" buildq cargo test || job_status=$?
# 前景のコマンドとその子孫がすべて終了した時点で削除する
rm -rf -- "$job_tmp"
exit "$job_status"
```

一時領域には実行可能なボリュームを選ぶ。
rustdoc は一時領域で doctest の実行ファイルを生成・起動するため、
`noexec` マウントは buildq と無関係に失敗の原因となる。
例の `/var/tmp` も実際のマウント設定を確認する。
一時ディレクトリは Git チェックアウトと `BUILDQ_CACHE` の外に置く。
任意のジョブの一時ファイルはプールメンバーではなく、キャッシュ予算に含まれず、GC の管理対象でもない。
使用量の監視、保持期限の制限、中断後の残骸の削除は呼び出し元が担う。
残骸もプロセスツリー全体の終了を確認してから削除する。

バックグラウンドの子孫を残すコマンドは、管理側でツリー全体の終了を待ってから削除する。
シェルの EXIT trap や最初のコマンドの終了だけでは、その条件を満たしたことにならない。
キャンセル時も確認が終わるまで一時ディレクトリを保持する。
この手順は継承するパスを一意にするものである。
TMPDIR/TMP/TEMP を無視するプログラムや別の固定パスを共有する処理には、個別の分離が必要である。
[Container Process Lifecycle](development.ja.md#container-process-lifecycle) を参照する。

## Nested Cargo

build script は、親のプール target を継承せずに別 Cargo ワークスペースの helper をビルドできる。
通常の helper target はプール外に残る。
見つかった既定ディレクトリは `cache ls` の外部容量として表示し、GC の対象にはしない。
親の予算とリースで管理したい場合は、`OUT_DIR` 配下を明示する。

```rust
let target = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
    .join("helper-target");
let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
    .args(["build", "--manifest-path", "helper/Cargo.toml", "--target-dir"])
    .arg(target)
    .status()
    .unwrap();
assert!(status.success());
```

## mr-boxington (mbx)

`[cargo] backend = "mbx"` を指定すると、キューを通る Cargo ジョブを [mr-boxington](https://github.com/jdx/mr-boxington) 経由で実行する。
mbx は共有ストアから一致するコンパイラ出力を復元する。
この設定はオプトインであり、デフォルトでは本物の Cargo を直接実行する。

```toml
[cargo]
backend = "mbx"
# mbx = "/absolute/path/to/mbx"   # デフォルト: PATH 上の `mbx`
```

責務は次のように分担する。

- buildq は、ジョブの受け入れ、プール target の配置、内容ベースのソース無効化、`cargo run` のステージング、自身の GC を受け持つ。
  各ジョブに `MBX_TARGET_VIEWS=0` と `MBX_TARGET_LANES=0` を渡すため、mbx は managed target も check lane も作らない。
- mbx はコンパイラをラップする。
  buildq は解決した本物の Cargo を `CARGO` として渡し、`+toolchain` もそのまま転送する。
  そのため、本物の Cargo は `+toolchain` を受け付けるもの（rustup や mise のプロキシ）でなければならない。
  mbx はサブコマンドの前にそれ以外の引数を受け付けない。
  そのため buildq は、Cargo のグローバルオプション（`--config`、`--color`、`-v`、`--locked` など）を組み込みサブコマンドまたは `clippy` の後ろへ移す。
  外部サブコマンドの前にある場合や `-C` を使う場合は移せないので、そのジョブは mbx を使わずに本物の Cargo で実行し、その旨を表示する。
  Cargo のエイリアスは展開してから mbx に渡す。
  そのため、mbx のコマンドと同じ名前のエイリアス（`gc`、`cache`、`doctor` など）でも Cargo が実行される。
- mbx を使うのはキューを通るジョブだけである。
  軽いコマンド、ジョブ内のネストした Cargo、buildq 自身の `cargo metadata` による調査は、本物の Cargo を直接実行する。

ループする構成や、キャッシュが黙って無効になる構成は拒否する。
具体的には、`cargo.rustc_wrapper` にパスを指定した場合（mbx はそのラッパーに処理を委ね、キャッシュしない）、本物の Cargo が mbx かその Cargo shim に解決される場合、`mbx` のパスが buildq 自身を指す場合である。
`rustc_wrapper` は `"none"`（デフォルト）か `"keep"` にする。
`"keep"` の場合でも、環境変数に `RUSTC_WRAPPER` が設定されていれば mbx はキャッシュしない。

PATH 上の Cargo 自体が mbx を実行する場合（mbx の Cargo shim、ラッパースクリプト、それらを指す mise の shim）、mbx は自分自身をラップしてしまう。
buildq 自身の Cargo の調査も mbx を経由し、mbx が worktree に管理用の `target` ビューを作ってしまう。
そのため buildq は、ジョブと調査の両方に `rustup which cargo` が返すツールチェーンの cargo を使い、その旨を表示する。
rustup がない場合や、`cargo.real` / `BUILDQ_REAL_CARGO` がそのような Cargo を明示している場合は、これらの設定名を示してジョブを止める。

mbx は、ストア、非共有のインクリメンタル状態、コレクタを独自に持ち、これらは `cache.budget` の対象外である。
ボリュームの容量は両方を見込んで確保するか、`MBX_GC_AUTO` と mbx の予算を明示的に設定する。
`buildq cache ls` と `buildq gc` が報告・回収するのは buildq のプールだけである。
両者を合わせたストレージの集計と GC の協調は、別途扱う。

## Cache Budget

デフォルトの `accounting = "allocated"` は、ファイルの割り当て量を使う。
メンバー・実行用コピー・ゴミ箱の各項目内でハードリンクの重複を除き、スパース領域は含めない。
重複を除いたファイル長で制限したい場合は `[cache] accounting = "logical"` を指定する。
`cache ls`・`gc`・`top`・`config` で集計基準を確認でき、JSON には `accounting` を含める。
ディレクトリのメタデータは含めず、別項目にまたがる共有領域は保守的に計上する。

`[cache] budget` はキャッシュの保持容量を制御する。実行中ビルドの最大サイズではない。
使用中の成果物は保護する。
残っている超過量と理由は `buildq cache ls --refresh` または `buildq gc --json` で確認できる。
使用中の成果物を回収できない場合、GC は予算超過のまま成功終了する場合がある。

完了メンバーが予算全体より大きい場合は、GC で回収されて次回もコールドビルドになる可能性を警告する。
作業に必要なキャッシュを保持できるよう `budget` を増やすか、成果物を削減する。
大きすぎる未使用メンバーにも保持の例外は設けない。
別の `min_free` ガードはディスク圧迫時の開始拒否・停止を引き続き行う。
`budget` を増やしても無効にはならない。
詳細は [Disk Budget & Garbage Collection](architecture.ja.md#4-disk-budget--garbage-collection) を参照する。

`cargo run` がネイティブ依存のためメンバーを保持する場合は、その旨を表示する。
run 待機中・実行中とも GC から保護し、残る予算超過を報告する。
長時間動くネイティブアプリは、ビルドに使えるメンバー数を減らす場合がある。
保持する分も見込んで `cache.max_members` を設定するか、アプリを終了して解放する。
ビルドスロットはアプリ実行前に解放する。

## Member Deduplication

GC は、未使用メンバー間でバイト単位に同一のビルド成果物を共有する。
同じツールチェーンであれば、別リポジトリのメンバーとも共有する。
共有したファイルの内容・mode・mtime は変わらないため、Cargo は引き続き最新と判断する。
`cache ls` はプールごとの共有量を表示し、予算では共有分を1回だけ数える。
`buildq gc --dry-run` も同じように（`dedupe_max_read` の範囲で）ファイルを比較し、共有後の使用量から追い出しを見積もる。

```sh
buildq cache dedupe --dry-run   # ファイルを変更せず重複量を計測
buildq cache dedupe             # `[cache] dedupe` に関係なく今すぐ共有
```

| 設定 | デフォルト | 説明 |
| :--- | :--- | :--- |
| `dedupe` | `"auto"` | `"off"` で GC 中の実行を無効化 |
| `dedupe_min_size` | `"64KiB"` | これより小さいファイルは比較しない |
| `dedupe_max_read` | `"8G"` | 1回の比較で読む上限 |

共有には reflink が必要である。
Linux では btrfs・`reflink=1` の XFS・bcachefs、macOS では APFS が対応する。
それ以外では `unsupported` と報告して何も変更しない。dry-run での計測は可能である。
詳細は [Cross-Member Deduplication](architecture.ja.md#cross-member-deduplication) を参照する。

## Project Cache Cleanup

`cargo clean` は通常の Cargo の選択規則を維持する。
`-p`・`--release`・`--profile`・`--target`・`--manifest-path` は、ローカルまたは明示した target のみに作用する。
プールは変わらない旨を buildq が表示する。
target-dir の設定と明示指定は通常の優先順位で解決する。
管理キャッシュと重なるパスは拒否し、使用中リースの迂回を防ぐ。

現在の Git リポジトリのプールを、全 worktree・全ツールチェーンにわたって掃除する場合は次を使う。

```sh
buildq cache clean --dry-run
buildq cache clean
buildq cache clean --json
```

`--manifest-path` で別の場所を掃除した場合も、意図するリポジトリ内から実行する。
このコマンドは未使用メンバー全体を対象とし、Cargo のパッケージ／profile 指定は受け付けない。
メンバーや選択処理が使用中、またはアクセス不能なプールはスキップし、終了コード `1` を返す。
別ツールチェーンの未使用プールは、すでに掃除されている場合がある。
対象ジョブの終了を待つか停止してから再試行する。ロックファイルは保持する。

全リポジトリの未使用プールをまとめて掃除する場合は、任意のディレクトリから次を使う。

```sh
buildq cache clean --all --dry-run
buildq cache clean --all
```

使用中のプールは同様にスキップする。

`--all` を付けなければ別リポジトリは削除しない。実行用コピーと外部 target は常に削除しない。
実行用コピーは GC が管理し、自分の外部 target は通常の Cargo clean で掃除する。
レポートの before/after は管理キャッシュ全体の値であり、actions はこのリポジトリの削除メンバーを示す。
削除失敗時も非ゼロで終了する。

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
