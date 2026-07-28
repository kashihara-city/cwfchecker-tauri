# CreateWebFlowChecker — Tauri / no npm

## 概要

インフォテック社のワークフローシステム
[Create!Webフロー](https://www.createwebflow.jp/)の処理待ち案件を定期確認する、
Windows向けデスクトップアプリケーションです。

旧Electron版
[kashihara-city/cwfchecker](https://github.com/kashihara-city/cwfchecker)
の動作を、Rust、Tauri、静的HTML/CSS/JavaScriptで再実装しています。
単独exeで動作し、設定値はレジストリで管理するため、GPOによる一斉デプロイ等も可能です。

## 何を解決し（避け）ようとしているか

- メールやチャットの単発通知では、今何件溜まっているのか分かず、確認が後回しにする（後回しにされる）
- 確認を後回しにしているうちに忘れる（忘れられる）
- 添付ファイルをダウンロードして開かないといけないのが億劫
- 結局決裁箱に溜まる紙のほうが見やすい、紙決裁よりのほうが回議が速いと言われる。ワークフローシステムが普及しない。
- 面倒なインストール、個別設定作業、ポートレット表示の際のワークフローシステムとグループウェアのID不一致、DLL配布、長大なサプライチェーン

## 動作要件

- Windows 11
- Microsoft Edge WebView2 Runtime
- Create!Webフローのポートレット表示オプションが利用可能であること
- ポートレット画面のHTTPまたはHTTPS URLを指定できること

本アプリはインフォテック社の公認・許諾を受けた製品ではありません。
利用環境のCreate!Webフローに対する動作確認を行ったうえで使用してください。

## 主な機能

- タスクトレイへの常駐し、処理待ち案件を確認
- 案件がある場合、メイン画面のポップアップまたはWindows通知バーで通知
- 案件確認・決裁画面をアプリ内で完結
- ショートカットによるメイン画面の表示・非表示

## 使用方法

1. `CreateWebFlowChecker.exe`を起動します。
2. 初回起動時に表示される設定画面、または「メニュー」→「設定画面」を開きます。
3. ID、PW、CWFAddressなどを入力して「設定反映」を押します。
4. アプリが再起動し、ポートレット画面を読み込みます。
5. ショートカットキー、タスクトレイ左クリック、またはタスクトレイの「表示」で画面を表示します。
6. 「メニュー」→「アプリ終了」またはタスクトレイの
   「電子決裁確認アプリ終了」で完全に終了します。

ウィンドウの閉じるボタンはメイン画面を非表示にします。アプリはタスクトレイで動作を継続します。

## 設定とその保存場所

通常のアプリ設定とWindows通知バー用の設定は、現在のWindowsユーザーの
レジストリ（`HKEY_CURRENT_USER`）へ保存します。
管理者権限は不要です。

```text
HKCU
└─ Software
   ├─ KashiharaCity
   │  └─ CwfChecker
   │     ├─ 一般の設定
   │     └─ GPO管理用の設定
   │
   └─ Classes
      └─ AppUserModelId
         └─ jp.lg.city.kashihara.cwfchecker
            └─ 通知バー用の設定
```

### 一般の設定

アプリが保存する一般的な設定です。

```text
HKEY_CURRENT_USER\Software\KashiharaCity\CwfChecker
```

| 値名                     | 設定画面の項目                     | 内容                                            | 初期値・制約          |
| ------------------------ | ---------------------------------- | ----------------------------------------------- | --------------------- |
| `AdServer`               | AD Server                          | Active Directoryサーバー名                      | 環境に応じて指定      |
| `CwfAddress`             | CWFAddress                         | ポートレット画面のURL                           | HTTPまたはHTTPS       |
| `IntervalMinutes`        | 確認間隔                           | 自動更新間隔（分）                              | 15～360分、初期値15分 |
| `NotifyByBar`            | ポップアップせず通知バーでお知らせ | 案件発見時にメイン画面ではなくWindows通知を表示 | 初期値OFF             |
| `Shortcut`               | ショートカットキー                 | 表示・非表示を切り替えるキー                    | 初期値`F3`            |
| `SchemaVersion`          | ―                                  | 設定形式のバージョン                            | アプリが自動設定      |
| `LegacyMigrationVersion` | ―                                  | 旧設定を一度だけ移行したことを示すマーカー      | 移行時に自動設定      |
| `AppVersion`             | ―                                  | 起動したアプリ自身のバージョン                  | 起動時に自動設定      |

複数キーのショートカットは`SHIFT+F2`のように、修飾キーとキーを`+`で区切ります。

設定保存に失敗した場合、設定画面を閉じたりアプリを再起動したりしません。

### 通知バー用の設定

アプリが保存する通知バー用の設定です。

```text
HKEY_CURRENT_USER\Software\Classes\AppUserModelId\jp.lg.city.kashihara.cwfchecker
```

| 値名          | 内容                                             |
| ------------- | ------------------------------------------------ |
| `DisplayName` | 通知に表示するアプリ名（`CreateWebFlowChecker`） |
| `IconUri`     | 通知用ICOの絶対パス                              |

起動したexeと同じフォルダーに`CreateWebFlowChecker.notification.ico`が
なければ、exeに埋め込まれたアイコンの自動生成を試みます。生成できた場合は
その絶対パスを`IconUri`へ登録します。

### IDとパスワード

IDとパスワードは通常設定のレジストリには保存せず、
Windows資格情報マネージャーの1組の汎用資格情報として保存します。

```text
ターゲット名: KashiharaCity.CwfChecker
ユーザー名:   設定画面のID
資格情報:     設定画面のPW
```

保存にはWindowsのCredential APIを使用し、保存直後に読み返して内容を確認します。
設定画面でPWを空欄にした場合は保存済みPWを維持します。
IDを変更する場合はPWの再入力が必要です。
`UseSAMLAuth=1`の環境でも利用者が入力したIDを保存します。SAML認証時は保存した
ID/PWをPOSTせず、実行時だけ`SAML/SAML`を生成します。SAML認証ではPWを空欄のまま
保存できます。IDも空欄なら資格情報を新規作成・削除せず、ほかの設定だけを反映します。

### GPO管理用の設定（一般設定と一部共通）

Tauri版の配布前にGPOで設定を管理する場合は、主に次の項目を設定します。

| 値名             | 種類        | 設定内容                                                                                                                               |
| ---------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `CwfAddress`     | `REG_SZ`    | 実際のCreate!WebフローのポートレットURL                                                                                                |
| `AdServer`       | `REG_SZ`    | 通常認証で使用するAD Server。SAMLでは省略可能                                                                                          |
| `UseSAMLAuth`    | `REG_DWORD` | `1`ならSAML認証、未設定または`0`なら通常認証。アプリの設定画面からは変更しません                                                        |
| `SchemaVersion`  | `REG_DWORD` | `1` 固定。無い場合、一般のアプリ設定は未設定として扱われます                                                                           |
| `LatestVersion`  | `REG_SZ`    | 最新バージョン（オプション、それより古いバージョンを起動するとメニューに「⬆ アップデートあり」と表示されます）                                                         |
| `MinimumVersion` | `REG_SZ`    | 最低バージョン（オプション、それより古いバージョンを起動すると現在のバージョンと必要な最低バージョンを表示し、終了します）                                               |

通常認証で資格情報がない端末では設定画面が開き、利用者がIDとPWを入力します。
`UseSAMLAuth=1`では資格情報がなくても設定画面を開かず、`SAML/SAML`をPOSTします。
このとき確認間隔、通知設定、ショートカットが未登録なら、旧Electron版の移行後に
既定値だけをレジストリへ補完します。移行済みの値やGPO配布値は上書きしません。
旧設定が残っている場合は、GPOの有効な`CwfAddress`と`UseSAMLAuth`を維持しながら
旧ID、AD Server、PWなどを一度だけ移行します。旧設定側の項目が空または欠落していれば、
対応するGPO配布値を維持します。SAML環境では旧ID/PWも資格情報へ移行して保持しますが、
認証時のPOSTには`SAML/SAML`を使用します。
レジストリに`Id`が存在していても参照・変更せず、資格情報側のIDを使用します。

## 旧Electron版からの移行

未移行の旧設定が存在する場合、Rust版の通常設定の有無にかかわらず一度だけ読み込みます。

```text
%APPDATA%\createwebflowchecker\config.json
```

- AD Server、確認間隔、通知設定、ショートカットをレジストリへ移行
- IDとPWを1組のWindows資格情報へ移行
- 旧設定側で空または欠落している項目は、完成済みのRust版設定があれば維持
- Rust版に有効なCWFAddressが既にあれば維持し、なければ旧CWFAddressを移行
- 同じIDの現行資格情報があれば最優先で維持
- 現行資格情報がなければElectron `safeStorage`の`encpw`、次に
  旧旧keytar資格情報`cwfchecker/<ID>`の順でPWを移行
- Electronの`v10`/`v11`形式では同じフォルダーの`Local State`から暗号鍵を読み、
  Windows DPAPIとAES-256-GCMで認証付き復号
- ID/PWをWindows資格情報マネージャーの`KashiharaCity.CwfChecker`へ保存
- 書き込み後に通常設定と資格情報を読み返して確認
- `LegacyMigrationVersion=1`を最後に保存し、認証前に再起動しても再移行しない

移行直後には旧ファイルを削除しません。Create!Webフローのページで認証成功を確認した後、
旧`config.json`と旧旧keytar資格情報を削除します。
旧設定が壊れていても有効なRust版設定があれば、警告を表示して現在の設定で起動を続けます。

## フッター画像

Create!Webフローのサーバー上で、次の画像を確認します。

```text
XFV20/manual/user/_images/cwfchecker_footer01.jpg
...
XFV20/manual/user/_images/cwfchecker_footer05.jpg

XFV20/manual/user/_images/cwfchecker_footer01.png
...
XFV20/manual/user/_images/cwfchecker_footer05.png
```

上記に対応するサーバのパスは、

```text
 /usr/local/CREATE_HOME/Tomcat/webapps/XFV20/manual/user/_images
```

です。

存在する画像から1枚をランダムに選び、ポートレット画面下部へ表示します。
画像は大小にかかわらずウィンドウ幅へ合わせ、縦横比を維持します。

## 一時データとダウンロード

- WebView2の一時データ：
  `%TEMP%\KashiharaCity\CwfChecker\WebView2\<プロセスID>`
- 添付ファイル：
  `%USERPROFILE%\Documents\cwf_downloads`

WebView2の一時データは起動時に作り直します。
案件画面でダウンロードしたファイルはWindowsの関連付けアプリで開き、
案件画面をすべて閉じた際に、`cwf_downloads`内のファイルとサブフォルダーをすべて削除します。
残したい添付ファイルや展開内容は、案件画面を閉じる前に別のフォルダーへ移してください。
本アプリは、Create!Webフロー側で添付可能なファイル形式が制限されていることを前提とします。
クライアント側の拡張子ブロックは追加防御であり、サーバー側の添付ポリシーを置き換えるものではありません。
誤実行を防ぐため、exe、msi、bat、cmd、PowerShell、JavaScriptなどの
実行・スクリプト形式はダウンロードと自動起動の対象外です。

## セキュリティ上の注意

- ID、PW、AD Server、表示種別は、公式ポートレット仕様に従って
  `application/x-www-form-urlencoded`のPOST本文で送信します。
  認証情報はURLのクエリ、静的HTML、ログへ出力しません。
- POST本文もHTTPでは暗号化されないため、`CWFAddress`にはHTTPSを使用してください。
- メイン画面と案件画面は、設定した`CWFAddress`と同じオリジンだけを
  WebView内に表示します。SAML認証中はIdPへのHTTPSリダイレクトを一時的に許可し、
  認証完了後は再び設定サーバーへ制限します。通常の外部HTTP・HTTPSリンクは
  既定ブラウザで開きます。
- パスワードそのものは設定画面のWebViewへ読み返さず、Windows資格情報
  マネージャー内で保持します。
- WebView2データはプロセス別の一時フォルダーへ置き、次回起動時に削除します。

## ビルドとテスト

Rustの安定版ツールチェーンが必要です。Node.jsとnpmは不要です。

```powershell
cd .\src-tauri
cargo test --locked
cargo build --release --locked
```

生成されるexe：

```text
src-tauri\target\release\CreateWebFlowChecker.exe
```

生成された`CreateWebFlowChecker.exe`を任意の場所へコピーして使用できます。
インストーラはありません。
Windows通知を確認するときは、`target\debug`または`target\release`の外へコピーして
起動してください。Tauriの通知プラグインはこれらのフォルダーを開発実行と判定し、
通知へアプリ識別子を設定しません。

## 主な依存ライブラリ

- Tauri 2
- `tauri-plugin-global-shortcut`
- `tauri-plugin-single-instance`
- `tauri-plugin-notification`
- `winreg`
- Windows API (`windows-sys`)
- `serde` / `serde_json`
- `url`

## ライセンス・注意事項

- 複製、変更、フォーク等は自由ですが、無保証です。
- 橿原市は、本プログラムを利用したことによる一切の損害に関知しません。
- 本プログラムは、インフォテック社の公認・許諾を受けていません。
