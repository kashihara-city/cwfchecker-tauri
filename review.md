# cwfchecker-tauri コードレビュー

対象: `src-tauri/src/*.rs`, `web/*`, `tauri.conf.json`, `capabilities/local.json`
実施日: 2026-07-26

## 総評

ElectronからTauriへの移植として、資格情報（Windows資格情報マネージャー）とアプリ設定（レジストリ）を
分離し、パスワードをURL・ログ・WebViewのIPCへ一切露出させない設計になっている点は良い。
オリジン検証、ダウンロード時の拡張子ブロック、`document.title`を介した一方向レポートなど、
XSS/資格情報漏洩を意識した対策が随所にあり、テストも重要なロジック（`is_blocked_download`、
`has_allowed_origin`、`parse_report_title`、`Settings::normalize`等）をカバーしている。

以下は指摘事項を重要度順に記載する。致命的な脆弱性は見つからなかった。

---

## 指摘事項

### 1. [Medium] SAML認証中フラグが「初回成功まで」開いたままになり、任意HTTPSリダイレクトを許可し続ける

- 該当箇所: `src-tauri/src/app.rs`
  - `authentication_in_progress` の初期値 `true`（[setup](src-tauri/src/app.rs#L925)）
  - `begin_portlet_load` で毎回 `true` に再セット（[app.rs:216-217](src-tauri/src/app.rs#L216-L217)）
  - `on_navigation` での許可判定（[app.rs:541-545](src-tauri/src/app.rs#L541-L545)）
    ```rust
    state_for_navigation
        .authentication_in_progress
        .load(Ordering::Acquire)
        && url.scheme() == "https"
    ```
  - `false` に戻るのは `process_page_report` で認証成功の目印（`auth_count > 0`）を検出した時のみ（[app.rs:865-867](src-tauri/src/app.rs#L865-L867)）

- シナリオ: ADサーバーの設定ミス、パスワード誤り、SAML IdP側の障害などで認証が一度も成功しない場合、
  `authentication_in_progress` は常駐アプリのプロセス生存期間中ずっと `true` のままになる。
  この間、`on_navigation` は **オリジンを一切確認せず** `https` であれば何でも遷移を許可する。
  結果として、CWFサーバーやSAML IdPが乗っ取られた場合・中間者攻撃を受けた場合に、
  資格情報を保持するWebViewが任意のHTTPSページへ誘導され得る（POSTは1回きりでも、
  その後のクリック操作等をフィッシングページに晒すリスクがある）。
  15分毎のタイマーや設定変更後の再起動でも、このフラグは「消える」のではなく「再武装される」だけなので、
  時間が経てば安全になるわけではない。

- 提案: 許可するリダイレクト先を「設定されたCWFサーバーと同一オリジン」または
  「事前に許可されたIdPオリジンのリスト」に限定する。時間的な制約が難しければ、
  最低限「1回のロード試行につき1回だけ」など、`begin_portlet_load`ごとに使い切りのカウンタにして、
  無期限の許可にならないようにする。

### 2. [Low] 複数スレッドから同時に`begin_portlet_load`が呼ばれるとPOST投入がスキップされ得る

- 該当箇所: `begin_portlet_load`（[app.rs:211-226](src-tauri/src/app.rs#L211-L226)）、
  呼び出し元は `show_main`（[app.rs:447-449](src-tauri/src/app.rs#L447-L449)、トレイ左クリック等）、
  `start_timer`（[app.rs:703-705](src-tauri/src/app.rs#L703-L705)、15分毎のバックグラウンドスレッド）、
  `reload`メニュー（[app.rs:963-966](src-tauri/src/app.rs#L963-L966)）の3箇所から、
  同期なしに同じ`AppState`のAtomicフラグを操作する。

- シナリオ: タイマー発火とユーザーのトレイクリックがほぼ同時に発生すると、両方が
  `post_navigation_armed`を`true`にして`window.navigate`を呼ぶ。`on_page_load`側は
  `swap(false, ...)`で一度だけフラグを消費するため（[app.rs:558-561](src-tauri/src/app.rs#L558-L561)）、
  2回目の遷移完了時にはフラグが既に消費されており、そのロードに対するPOSTスクリプトの注入
  （ID/PW投入）がスキップされる可能性がある。メモリ安全上の問題はないが、
  「更新したのにログイン画面のまま」という体感不具合につながり得る。

- 提案: 必須ではないが、`begin_portlet_load`をミューテックスや単一のワーカースレッド経由に
  直列化すると、この手のレースを避けられる。

### 3. [Low] `cleanup_directory`はトップレベルのファイルのみ削除し、サブディレクトリを残す

- 該当箇所: [app.rs:348-358](src-tauri/src/app.rs#L348-L358)、呼び出し元は
  案件ウィンドウが全て閉じた時のダウンロードフォルダ掃除（[app.rs:1019-1023](src-tauri/src/app.rs#L1019-L1023)）。

- シナリオ: ダウンロードしたzipをエクスプローラー等でその場に展開した場合など、
  `download_dir`直下にサブフォルダが作られると、`cleanup_directory`は`candidate.is_file()`
  のみを消すため、そのフォルダとその中身は残り続ける。実害は小さいが、意図（毎回まっさらにする）
  とはズレがある。

- 提案: 気になる場合は`fs::remove_dir_all(path)`後に`fs::create_dir_all(path)`し直す方が、
  コメントの意図（次回起動時に前回のダウンロードを残さない）に忠実。

### 4. [Info] `decode_password_blob`のUTF-8/UTF-16LE判定はヒューリスティックである

- 該当箇所: [credentials.rs:49-64](src-tauri/src/credentials.rs#L49-L64)

- 内容: keytar由来のUTF-8を優先し、UTF-8として不正な場合のみUTF-16LEとして解釈する。
  理論上、UTF-16LEで書かれたパスワードがたまたま有効なUTF-8バイト列にもなる場合
  （高いバイト値の組み合わせ次第）、誤ってUTF-8として復号される可能性はゼロではない。
  ただし移行対象は旧ツール特有の既知フォーマットであり、実運用のASCIIパスワードでは
  まず起こらないため、致命的ではない。設計上のトレードオフとして記録のみ。

### 5. [Medium] `settings::read()`が`normalize()`を通していない

- 該当箇所:
  - `Settings::normalize()`のdocコメントは「読み書きの境界で必ず通す」ことを意図している
    （[settings.rs:43-47](src-tauri/src/settings.rs#L43-L47)）。
  - しかし実際に呼んでいるのは`write()`のみ（[settings.rs:122-126](src-tauri/src/settings.rs#L122-L126)）で、
    `read()`（[settings.rs:88-119](src-tauri/src/settings.rs#L88-L119)）は生の値をそのまま返す。
  - `migration::load_or_migrate()`はその結果を検証なしにアプリの実行時`Settings`として採用する
    （[migration.rs:57-63](src-tauri/src/migration.rs#L57-L63)）。

- シナリオ: 通常のUI経由（`save_settings`）の保存では`normalize()`済みの値しかレジストリに
  入らないため問題は顕在化しない。しかしレジストリは同一ユーザー権限を持つ他プロセスや
  regedit等でも書き換え可能な「信頼境界の外側」であり、`IntervalMinutes=0`や末尾に空白の付いた
  `Shortcut`、認証情報入りの`CwfAddress`が手動で入っていても、そのまま状態に載り、
  設定画面にもその値がそのまま表示される。ドキュメントが意図する「読み書きの境界」の
  片側（読み込み）が実装から抜けている。

- 提案: `read()`の最後で`.normalize()`を呼び、失敗時はスキーマ不一致と同様に`None`扱い
  （＝`Settings::default()`へフォールバックし再設定を促す）にする。既存の
  「壊れていたら安全に初期化へフォールバックする」設計と一貫する。

### 6. [Low〜Info] JS注入コードがRust文字列リテラルに直書きされている

- 該当箇所: `webview_script`（[app.rs:228-327](src-tauri/src/app.rs#L228-L327)）、
  `portlet_post_script`（[app.rs:167-208](src-tauri/src/app.rs#L167-L208)）。

- 内容: `format!`でJSをそのまま埋め込んでおり、`{{`/`}}`のエスケープが必要で読みにくく、
  エディタのJS構文ハイライト・Lint・整形の恩恵を受けられない。

- 提案: JSロジック本体は静的な`.js`ファイルへ切り出し`include_str!`で取り込み、
  動的な値（origin、bootstrap URL、POSTフィールド等）は`window.__cwfConfig = {json};`
  のような1行だけをRust側で`serde_json::to_string`により生成して先頭に注入する形にすると、
  現状と同じ安全性（値のJSONエスケープ）を保ったまま、静的ロジックを普通のJSとして
  扱えるようになる。

### 7. [Info] 設定保存は完全なACIDトランザクションではない（サーガ的な補償アクション）

- 該当箇所: `save_settings`（[app.rs:733-815](src-tauri/src/app.rs#L733-L815)）。
  資格情報を書いて検証→レジストリを書いて検証→レジストリ失敗時のみ資格情報をロールバック、
  という順で処理している。

- シナリオ: 資格情報の書き込み・検証が終わった直後からレジストリの書き込み・検証が
  完了するまでの間にプロセスがクラッシュ／強制終了すると、ロールバックコードが走らないため、
  Windows資格情報マネージャーに「新しい設定のidと紐付かない孤立したエントリ」が残り得る。
  ただし各所（`configured_url`等、[app.rs:138-141](src-tauri/src/app.rs#L138-L141)）で
  「保存済み資格情報のusernameと現在のsettings.idが一致する場合のみ使う」という照合を
  必ず挟んでいるため、実害は「間違ったパスワードが誤って使われる」ことではなく、
  「孤立エントリが残り、ユーザーが再度PW入力を求められる」程度に留まる。

- 提案: `CredWriteW`とレジストリ書き込みをまたぐ真の分散トランザクションはOSレベルで
  存在しないため、完全なアトミック性の実現は現実的ではない。現状の
  「サーガ＋username突合せガード」は妥当な落としどころと考えられ、優先度は低い。
  気にする場合は起動時に「設定のidと一致しない孤立資格情報を検出して掃除する」処理を
  追加する程度で十分。

---

## 良い点（参考）

- パスワードをレジストリやログ、WebViewのJSへ渡さず、Windows資格情報マネージャー経由に限定している。
- `on_navigation`/`on_new_window`でオリジンを厳格に比較し、外部リンクは`ShellExecuteW`で
  既定ブラウザへ逃がしている（[app.rs:589-635](src-tauri/src/app.rs#L589-L635)）。
- ダウンロードは拡張子ブロックリストに加え、末尾の空白・ピリオド正規化、代替データストリーム
  （`:`）対策まで行っている（[app.rs:382-400](src-tauri/src/app.rs#L382-L400)）。
- 設定・資格情報の書き込みは「書いてから読み戻して一致を確認し、失敗したら元に戻す」
  というパターンで統一されている（`save_settings`、`migration::load_or_migrate`）。
- レジストリの`SchemaVersion`を最後に書き込むことで、書き込み途中断でも不完全な設定を
  読み込まないようにしている（[settings.rs:130-146](src-tauri/src/settings.rs#L130-L146)）。
- 単体テストが「壊れたら困る」境界（オリジン判定、危険拡張子、レポート文字列のパース、
  ショートカット記法検証）を的確にカバーしている。

---

## 指摘の再評価と対応結果（2026-07-26）

レビュー後にコードと旧keytar実装を再確認し、以下のとおり判断・対応した。

### 1. SAML認証中のHTTPS遷移許可

- **今回は保留。**
- 元の指摘にある「`auth_count > 0`を検出するまでフラグが閉じない」という説明は誤りで、
  設定先オリジンから有効なページレポートを受信した時点で、認証成功の有無にかかわらず
  `authentication_in_progress`は`false`になる。
- 一方、SAML IdP上のエラーページ等に留まった場合はHTTPS許可が継続するため、
  認証先オリジンの制限やタイムアウトは今後の検討対象とする。

### 2. ポートレットPOST投入の競合

- **対応済み。**
- 各bootstrap遷移に世代番号を付与し、最新世代と一致するロードだけがPOST処理を
  1回取得できるようにした。
- 複数スレッドからの`navigate`要求順と世代更新順は専用Mutexで直列化し、
  ナビゲーションコールバックが読む状態のMutexとは分離している。
- 古いロード完了イベントや同一世代の重複イベントはPOSTを実行しない。

### 3. ダウンロードフォルダーのサブディレクトリ

- **方針を変更して対応済み。**
- 案件ウィンドウをすべて閉じた際、`cwf_downloads`直下のファイルだけでなく、
  サブフォルダーとその内容も削除する。
- 1件の削除に失敗しても残りの削除を続行し、最初のエラーを呼び出し元へ返す。
- READMEにも、残したいファイルは案件画面を閉じる前に別フォルダーへ移す旨を追記した。

### 4. 資格情報Blobの文字コード

- **UTF-8専用として対応済み。**
- 現行Rust版と移行元のkeytarはいずれもCredentialBlobへUTF-8を保存するため、
  UTF-16LEフォールバックを削除した。不正UTF-8は`InvalidData`として拒否する。
- 元の指摘にある「ASCIIパスワードではまず起こらない」という説明は誤りである。
  UTF-16LEのASCII文字列はNULを含む有効なUTF-8バイト列にもなるため、
  旧ヒューリスティックではUTF-8として誤判定される。

### 5. 設定読込み時の正規化

- **対応済み。**
- `settings::read()`でも`Settings::normalize()`を通し、空白、最低更新間隔、
  空ショートカット等を正規化する。
- 不正URLや不正ショートカットは`InvalidData`とする。
- 「現行設定なし」と「現行設定が不正」を区別し、不正な現行設定で旧Electron版の
  再移行が走らないようにした。不正時はエラーを表示し、安全な初期値で設定画面を開く。

### 6. JavaScriptの切り出し

- **対応済み。**
- ページ走査処理を`src-tauri/scripts/cwf-scan.js`、POST処理を
  `src-tauri/scripts/portlet-post.js`へ切り出し、`include_str!`で実行ファイルへ埋め込む。
- 動的な設定値はJSONとしてIIFEの引数へ渡し、パスワードをグローバル変数へ保持しない。
- なお、元の「パスワードをWebViewのJSへ渡さない」という良い点の記述は不正確である。
  パスワードはPOST開始用ローカルページのJSで一時的に扱うが、設定画面のIPC、URL、
  永続ログには渡さない。

### 7. 設定保存失敗時の補償処理

- **レジストリ側も含めて対応済み。**
- 元の実装が復元していたのはWindows資格情報だけで、レジストリ設定は復元していなかったため、
  「現状がサーガになっている」という評価は不正確だった。
- 保存前に以前の設定と、完成済みレジストリ設定が存在したかを保持する。
- レジストリ保存または保存後検証に失敗した場合は、Windows資格情報の復元と独立して、
  以前のレジストリ設定を書き戻して検証する。以前の設定がなければ途中生成キーを削除する。
- 片方の復元に失敗しても、もう片方の復元は必ず試し、各失敗内容を利用者へ返す。
- プロセスクラッシュをまたぐ完全なACID性は引き続き保証しない。

### 検証

- `cargo audit`完了（脆弱性エラーなし、既存依存に許容済み警告あり）。
- `cargo test --locked`：20件成功。
- `cargo clippy --locked --all-targets -- -D warnings`：成功。
- `cargo build --locked`：成功。
- 切り出したJavaScript：構文確認成功。
