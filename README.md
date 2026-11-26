# CloneGenealogyWeaver

Git の履歴を版ごとにたどって Type-3 クローンのペアを検出し，版と版の間のペアのイベントを SQLite に記録するツールです．記録から，ペアの系譜のファイルを作れます．実行ファイルの名前は `cgweave` です．

対応する OS は Linux だけです．

## ビルド

Rust の版は `rust-toolchain.toml` で決まります．rustup があれば，その版が入ります．

```sh
cargo build --release --locked
```

実行ファイルは `target/release/cgweave` にできます．`cgweave --version` で，ツールの版と組み込んだプラグインの名前が出ます．ツールの版はコミットのハッシュを含み，コミットしていない変更があれば最後に `.dirty` が付きます．研究に使う記録は，コミットした版で動かしてください．

テストは次のとおりです．

```sh
cargo test --locked
```

MSCCD と CLAIM を実際に動かすテストは，それぞれが見つからなければ飛ばして，飛ばしたことを出します．

- MSCCD は，環境変数 `CGWEAVE_TEST_MSCCD_DIR` のディレクトリから探します．
- Java 17 で止まることを確かめるテストは，環境変数 `CGWEAVE_TEST_JAVA17` に Java 17 の `java` を書いたときだけ動きます．
- CLAIM は，`plugins/claim/.venv/bin/python` の仮想環境を使います．

## 外部のプログラムの用意

### MSCCD

検出器の `msccd` は，MSCCD の tokenizer と detector の 2 つの jar を直接呼びます．

```sh
git clone https://github.com/zhuwq585/MSCCD
git -C MSCCD checkout 6158751459a871709c70ca2777ca83ac8c40ce9d
```

- tokenizer は Java 18 以上で動きます．設定を確かめるときに `java -version` を読み，18 より前なら止まります．
- MSCCD のリポジトリの中は変えません．置いたディレクトリを，設定の `msccd_dir` に書きます．

### CLAIM

分類の `claim` は，`plugins/claim/` の CLAIM を Python で呼びます．仮想環境を作って，依存を入れてください．

```sh
python3 -m venv plugins/claim/.venv
plugins/claim/.venv/bin/pip install -r plugins/claim/requirements.txt
```

依存の版は `requirements.txt` のとおりに固定しています．設定を確かめるときに，仮想環境の版がこれと違えば止まります．

`plugins/claim/claim.py` は，CLAIM の replication package の `src/claim.py` を変えたもので，MIT で配布します．元の著作権と出典は，同じディレクトリの `LICENSE` と `NOTICE` にあります．

## 使い方

### グラフのファイル

記録する版を，1 行に 1 つ書きます．行の先頭が版のコミットで，空白で区切ってグラフの上の親の版を続けます．親の無い版は，コミットだけを書きます．`git rev-list --parents` の出力と同じ形です．ハッシュは小文字の 16 進数で 4 文字以上にし，`HEAD` やブランチの名前は使えません．親に書いた版も，どこかの行に版として書きます．

```
1a2b3c4
5d6e7f8 1a2b3c4
9a0b1c2 5d6e7f8
```

first-parent の列なら，次のように作れます．

```sh
git -C <repo> rev-list --first-parent --reverse <終わりの版> \
  | awk 'NR == 1 { print; p = $1; next } { print $1, p; p = $1 }' > graph.txt
```

### 設定のファイル

TOML で書きます．パスは，設定のファイルのあるディレクトリからの相対パスとして読みます．実行ファイルの項目は，`/` を含まない名前なら `PATH` から探します．

```toml
retries = 2
line_cache_mib = 1024

[detector]
name = "msccd"
timeout_secs = 600
msccd_dir = "/path/to/MSCCD"

[classifiers.claim]
python = "/path/to/CloneGenealogyWeaver/plugins/claim/.venv/bin/python"
claim_name = "owner.repository"

[classifiers.generated]

[classifiers.vendor]
```

- `[classifiers.<名前>]` の節を書いたプラグインだけを使います．どれも書かなければ，範囲は全部のファイルになります．
- 分類のプラグインは `claim`，`test_path`，`generated`，`vendor`，`fixture` です．
- MSCCD の項目の名前は，MSCCD の `config.json` に合わせています．書かなければ，`minTokens` 20，`minTokensForBagGeneration` 20，`detectionThreshold` 0.7，`maxRound` 4，スレッド数 8 と 8 です．
- tokenizer は 1 回の呼び出しに少なくとも 5 秒ほどかかるので，`timeout_secs` はそれより長くしてください．
- 知らない項目があれば，データベースを作る前に止まります．

### 記録する

```sh
cgweave record --repo <repo> --graph graph.txt --config config.toml --db events.db
```

- 元のリポジトリには書き込みません．解析用の複製を `events.db.clone/` に作り，記録が最後まで終わったら消します．置き場所は `--clone-dir` で変えられます．
- 記録の間は，元のリポジトリの履歴を書き換えたり，`git gc --prune` で古いオブジェクトを消したりしないでください．複製が元のリポジトリのオブジェクトを使います．
- 同じデータベースに記録できるのは，同時に 1 つだけです．

### 止まった記録を続ける

途中で止まった記録は，`status` で状態と続け方を見られます．

```sh
cgweave status --db events.db
```

最後の行に，続けるためのコマンドが出ます．

```sh
cgweave record --repo <repo> --graph graph.txt --config config.toml --db events.db --resume <版>
```

グラフのファイル，設定のファイル，ツールの版，Git の版は，止まる前と同じものを使ってください．違えば止まります．

### ペアを見る

```sh
cgweave pairs --db events.db --commit <版>
```

その版のペアを，1 行に 1 つの JSON で出します．版は短いハッシュでも，記録の中で 1 つに決まれば受け付けます．

### 系譜のファイルを作る

```sh
cgweave genealogy --db events.db --output genealogy.db
```

- 最後まで終わった記録だけを受け付けます．
- どの版も親が 1 つ以下のグラフで記録したものだけを受け付けます．first-parent の列やリリースの列です．
- 系譜のファイルが既にあれば止まります．書きかけのファイルは，隣に `.tmp-` の付いた名前で作り，書き終えてから名前を替えます．
- 系譜のファイルの表は，イベントのデータベースの番号を指します．SQLite で `ATTACH DATABASE 'events.db' AS events` とつないで引いてください．

## プラグイン

検出器と分類は，プラグインとしてツールに組み込みます．実行のときに外から読み込む仕組みは無いので，足すときも消すときもビルドし直します．組み込んだプラグインの名前は，`cgweave --version` で出ます．

### 種類

- 検出器は，版ごとに，範囲のファイルからクローンペアを返します．設定の `[detector]` で 1 つだけ選びます．
- 分類は，設定の `[classifiers.<名前>]` でいくつでも選べます．役割は 2 つあり，片方だけでも両方でも持てます．
  - 範囲を絞る分類は，版のファイルのうち残すものを返します．範囲は，どの分類も残したファイルだけになります．
  - ラベルを振る分類は，版のペアごとに，決めた値のどれかを返します．ラベルは，Disappear を除くペアイベントと，起点のペアごとに記録します．

| 名前 | 種類 | 中身 |
| --- | --- | --- |
| `msccd` | 検出器 | MSCCD でクローンペアを検出する |
| `claim` | 分類 | CLAIM が見つけたサービスのファイルに範囲を絞り，2 つの片が同じサービスなら `intra`，違えば `inter` を振る |
| `generated` | 分類 | 先頭の 32 KiB に，生成されたことを示す印のあるファイルを外す |
| `vendor` | 分類 | `vendor` という名前のディレクトリの下のファイルを外す |
| `test_path` | 分類 | 2 つの片のパスに `test` という語があるかで，`product`，`test`，`mixed` を振る |
| `fixture` | 検出器と分類 | JSON のファイルに書いたペアとサービスを返す．テストに使う |

版ごとに，ツールは次の順にプラグインを呼びます．分類どうしは，名前の辞書順に呼びます．

1. 分類の `scope` で範囲を決める．
2. 検出器の `detect` で，範囲のファイルからペアを検出する．
3. ラベルを振る分類の `classify` で，ペアにラベルを振る．

### プラグインを作る

プラグインは `src/plugin/` に 1 つのファイルで書き，`src/plugin/mod.rs` の trait を実装します．

```rust
pub trait Detector {
    fn effective_config(&self) -> serde_json::Value;
    fn detect(&mut self, call: &PluginCall, scope: &[Vec<u8>])
    -> Result<Vec<RawPair>, PluginError>;
}

pub trait Classifier {
    fn name(&self) -> &'static str;
    fn effective_config(&self) -> serde_json::Value;
    fn labels(&self) -> &'static [&'static str];
    fn scope(&mut self, call: &PluginCall, tree: &[TreeEntry]) -> Result<ScopeResult, PluginError>;
    fn classify(&mut self, pairs: &[Pair]) -> Result<Vec<&'static str>, PluginError>;
}
```

- `effective_config` は，既定の値を埋めた設定を返します．記録の `plugin` 表に書きます．
- `detect` は，範囲のファイルのパスを受け取り，ペアを返します．
  - 片は範囲の中のファイルに置きます．行は 1 から数えて終了行を含み，ファイルの行の数を超えないようにします．
  - 空白だけの片は書けません．
  - 類似度は 0 以上 1 以下にするか，無しにします．
- `name` は，設定の節の名前と同じにします．
- `labels` は，振るラベルの値の一覧です．空なら `classify` は呼ばれません．
- `scope` は，版の tree の一覧を受け取ります．範囲を絞るなら，残す通常のファイルのパスを `keep` に入れます．絞らないなら `keep` を `None` にします．
- `classify` は，直前の `scope` と同じ版のペアを受け取り，ペアと同じ数のラベルを同じ順に返します．`PluginCall` を受け取らないので，版の情報が要るなら `scope` で覚えておきます．
- `PluginCall` には，版のコミット，その版を置いた作業ツリー，呼ぶたびに作る空の一時ディレクトリ，時間の上限，割り込みが入っています．

失敗は `PluginError` で返します．

- `Failed` なら，設定の `retries` の回数までやり直します．使い切ると，その版を壊れた版として記録し，次の版へ進みます．
- `Abort` なら，記録を止めます．
- 出力が上の決まりを破ると，ツールは記録を止めます．やり直しはしません．

ほかに，次を守ります．

- 同じ入力には同じ結果を返します．記録を再開したときに，止まる前と同じ範囲になるようにするためです．
- 作業ツリーは変えません．一時ファイルは，`PluginCall` の一時ディレクトリに書きます．ツールは `scope` と `detect` を呼ぶたびに，作業ツリーが変わっていないかを確かめます．
- パスは，作業ツリーの根からの相対パスで，`/` で区切ったバイト列です．
- 外部のプログラムは，`src/process.rs` の `run_program` で動かします．時間の上限，割り込み，環境変数を，ほかのプラグインと同じに扱うためです．
- 設定の項目は，`Items::new` に知っている項目の名前を渡して読みます．知らない項目があれば止まります．
- 外部のプログラムを動かさないプラグインは，`refuse_timeout` で `timeout_secs` を拒みます．
- 外部のプログラムの版を設定のときに確かめるなら，`run_check` を使います．

雛形には次が使えます．

- 範囲を絞るだけの分類: `src/plugin/vendor.rs`
- ラベルを振るだけの分類: `src/plugin/test_path.rs`
- 外部のプログラムを動かすプラグイン: `src/plugin/msccd.rs`，`src/plugin/claim.rs`
- 一番小さい検出器: `src/plugin/fixture.rs` の `FixtureDetector`

### プラグインを足す

名前を `foo` とします．名前は表の名前の先頭にも使うので，英小文字と `_` で書きます．

1. `src/plugin/foo.rs` を作り，`src/plugin/mod.rs` の先頭に `pub mod foo;` を足します．
2. `src/plugin/mod.rs` の `classifiers` の `match` に，`"foo"` の腕を足します．検出器なら，`detector` の `match` に足します．
3. `src/version.rs` の `CLASSIFIERS` に名前を足します．検出器なら `DETECTORS` に足します．同じファイルの `VERSION_TEXT` にも，手で書き足します．どちらも名前の辞書順に並べます．
4. 分類なら，`src/store/mod.rs` の `classifier_tables` に名前を足します．
   - ラベルを振るなら，`schema/plugins/foo.sql` に `foo_pair_event` と `foo_root_pair` を作ります．形は `schema/plugins/test_path.sql` と同じにし，`label` の CHECK に `labels` と同じ値を書きます．`include_str!` の定数と `match` の腕を足し，2 つ目の値を `true` にします．
   - `ScopeResult` の `rows` で書く表も，同じファイルに作ります．名前は `foo_` で始めます．1 列目が版の番号の表は，`per_version` に入れます．再開のときに，止まった版より後の行を消すためです．
   - 表を持たないなら，`"generated" | "vendor"` の並びに足します．
5. `tests/cli.rs` の `--version` の行を直します．表を持つなら，`tests/schema.rs` の `PLUGINS_WITH_TABLES` と `plugin_table_rows` にも足します．
6. 新しいプラグインのテストを，`src/plugin/foo.rs` の中か `tests/` に書きます．
7. `src/`，`schema/`，`tests/` の外のファイルを使うなら，`build.rs` の `WATCHED` に足します．そのファイルを変えたときにも，ツールの版に `.dirty` が付くようにするためです．
8. 「設定のファイル」の分類の一覧と，この節の表に足します．

ビルドし直して，設定のファイルに `[classifiers.foo]` の節を書くと使えます．検出器なら，`[detector]` の `name` に書きます．

### プラグインを消す

- 使わないだけなら，設定のファイルから節を消します．記録を再開するときは止まる前と同じ設定のファイルが要るので，記録の途中では変えられません．
- ツールから消すときは，足す手順を逆にたどります．`src/plugin/<名前>.rs`，`pub mod` と `match` の腕，`src/version.rs` の名前，`classifier_tables` の腕と表の定義，テスト，README から消します．`plugins/` の下にファイルがあれば，そのディレクトリと，`build.rs` の `WATCHED` と `.gitignore` の行も消します．
- 消したプラグインで記録したデータベースは，`status` と `genealogy` では読めます．`pairs` と再開は，`no table definition is known for classifier` で止まります．そのプラグインを含む版のツールで読んでください．
- `fixture` は，結合テストの多くが使います．`claim` は，`tests/schema.rs` でラベルの表の形の基準です．この 2 つを消すときは，テストも書き直します．

## 終了コード

| コード | 意味 |
| --- | --- |
| 0 | 終わった |
| 1 | 誤りで止まった．理由は標準エラーに 1 行で出る |
| 130 | SIGINT，SIGTERM，SIGHUP で止まった |

## ファイルの置き場所

| 場所 | 中身 |
| --- | --- |
| `src/` | ソース．判定の計算は `src/core/`，プラグインは `src/plugin/` にある |
| `schema/` | イベントのデータベースと系譜のファイルの表の定義．`schema/plugins/` は分類のプラグインの表 |
| `plugins/claim/` | CLAIM の `claim.py`，入口の `entry.py`，依存の版，CLAIM の `LICENSE` と `NOTICE` |
| `tests/` | 記録の全体を `cgweave` で動かして確かめるテスト |

## ライセンス

MIT です．`LICENSE` にあります．`plugins/claim/claim.py` は CLAIM を元にしたもので，著作権と出典は `plugins/claim/LICENSE` と `plugins/claim/NOTICE` にあります．
