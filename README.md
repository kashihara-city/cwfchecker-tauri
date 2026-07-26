# CreateWebFlowChecker — Tauri / no npm

## 概要

インフォテック社のワークフローシステム
[Create!Webフロー](https://www.createwebflow.jp/)の処理待ち案件を定期確認する、
Windows向けデスクトップアプリケーションです。

旧Electron版
[kashihara-city/cwfchecker](https://github.com/kashihara-city/cwfchecker)
の動作を、Rust、Tauri、静的HTML/CSS/JavaScriptで再実装しています。
単独exeで動作し、設定値はレジストリで管理するため、GPOによる一斉デプロイ等も可能です。

## 動作要件

- Windows 11
- Microsoft Edge WebView2 Runtime
- Microsoft Visual C++ 再頒布可能パッケージ（x64版、`VC_redist.x64.exe`）
- Create!Webフローのポートレット表示オプションが利用可能であること
- ポートレット画面のHTTPまたはHTTPS URLを指定できること

`VC_redist.x64.exe`が未導入の環境では、本アプリを起動する前に
Microsoft Visual C++ 再頒布可能パッケージのx64版をインストールしてください。

本アプリはインフォテック社の公認・許諾を受けた製品ではありません。
利用環境のCreate!Webフローに対する動作確認を行ったうえで使用してください。

## 主な機能

- タスクトレイへの常駐し、処理待ち案件を確認
- 案件がある場合、メイン画面のポップアップまたはWindows通知バーで通知
- 案件確認・決裁画面をアプリ内で完結
- ショートカットによるメイン画面の表示・非表示

## 使用方法

1. Create!Webフローのポートレット表示オプションがブラウザで利用できることを確認します。
2. `CreateWebFlowChecker.exe`を起動します。
3. 初回起動時に表示される設定画面、または「メニュー」→「設定画面」を開きます。
4. ID、PW、CWFAddressなどを入力して「設定反映」を押します。
5. アプリが再起動し、ポートレット画面を読み込みます。
6. ショートカットキー、タスクトレイ左クリック、またはタスクトレイの「表示」で画面を表示します。
7. 「メニュー」→「アプリ終了」またはタスクトレイの
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
   │     └─ 通常のアプリ設定
   │
   └─ Classes
      └─ AppUserModelId
         └─ jp.lg.city.kashihara.cwfchecker
            └─ 通知バー用の設定
```

### 通常のアプリ設定

保存先：

```text
HKEY_CURRENT_USER\Software\KashiharaCity\CwfChecker
```

| 値名              | 設定画面の項目                     | 内容                                            | 初期値・制約         |
| ----------------- | ---------------------------------- | ----------------------------------------------- | -------------------- |
| `Id`              | ID                                 | Create!WebフローのログインID                    | 必須                 |
| `AdServer`        | AD Server                          | Active Directoryサーバー名                      | 環境に応じて指定     |
| `CwfAddress`      | CWFAddress                         | ポートレット画面のURL                           | HTTPまたはHTTPS      |
| `IntervalMinutes` | 確認間隔                           | 自動更新間隔（分）                              | 最低15分、初期値15分 |
| `NotifyByBar`     | ポップアップせず通知バーでお知らせ | 案件発見時にメイン画面ではなくWindows通知を表示 | 初期値OFF            |
| `Shortcut`        | ショートカットキー                 | 表示・非表示を切り替えるキー                    | 初期値`F3`           |
| `SchemaVersion`   | ―                                  | 設定形式のバージョン                            | アプリが自動設定     |

複数キーのショートカットは`SHIFT+F2`のように、修飾キーとキーを`+`で区切ります。

設定保存に失敗した場合、設定画面を閉じたりアプリを再起動したりしません。

### 通知バー用の設定

保存先：

```text
HKEY_CURRENT_USER\Software\Classes\AppUserModelId\jp.lg.city.kashihara.cwfchecker
```

| 値名          | 内容                                             |
| ------------- | ------------------------------------------------ |
| `DisplayName` | 通知に表示するアプリ名（`CreateWebFlowChecker`） |
| `IconUri`     | 通知用ICOの絶対パス                              |

起動したexeと同じフォルダーに`CreateWebFlowChecker.notification.ico`が
なければ、exeに埋め込まれたアイコンの自動生成を試みます。生成できた場合は
その絶対パスを`IconUri`へ登録します。書き込み禁止などで生成できない場合は
`IconUri`を登録せず、Windowsの汎用アイコンで通知します。

アプリ設定または通知バー設定のレジストリ操作に失敗した場合は、
処理名、対象キー、Windowsエラーを共通のメッセージボックスで表示します。
通知バー設定に失敗しても、アプリ本体の起動は継続します。

### パスワード

パスワードは通常設定のレジストリには保存しません。
Windows資格情報マネージャーの汎用資格情報として保存します。

```text
ターゲット名: KashiharaCity.CwfChecker
ユーザー名:   設定画面のID
資格情報:     設定画面のPW
```

保存にはWindowsのCredential APIを使用し、保存直後に読み返して内容を確認します。
設定画面でPWを空欄にした場合は保存済みPWを維持します。
IDを変更する場合はPWの再入力が必要です。

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

## 旧Electron版からの移行

Rust版の通常設定がまだ存在しない場合、次の旧設定を読み込みます。

```text
%APPDATA%\createwebflowchecker\config.json
```

- ID、AD Server、CWFAddress、確認間隔、通知設定、ショートカットをレジストリへ移行
- Electron `safeStorage`で暗号化された`encpw`をWindows DPAPIで復号
- 旧旧keytar資格情報`cwfchecker/<ID>`があれば移行元として使用
- PWをWindows資格情報マネージャーの`KashiharaCity.CwfChecker`へ保存
- 書き込み後に通常設定と資格情報を読み返して確認

移行直後には旧ファイルを削除しません。Create!Webフローのページで認証成功を確認した後、
旧`config.json`と旧旧keytar資格情報を削除します。

## 一時データとダウンロード

- WebView2の一時データ：
  `%TEMP%\KashiharaCity\CwfChecker\WebView2\<プロセスID>`
- 添付ファイル：
  `%USERPROFILE%\Documents\cwf_downloads`

WebView2の一時データは起動時に作り直します。
案件画面でダウンロードしたファイルはWindowsの関連付けアプリで開き、
案件画面をすべて閉じた際に、`cwf_downloads`内のファイルとサブフォルダーをすべて削除します。
残したい添付ファイルや展開内容は、案件画面を閉じる前に別のフォルダーへ移してください。
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
