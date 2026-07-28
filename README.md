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
- ポートレット画面のHTTPまたはHTTPS URLを指定できること（httpsを強く推奨）

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

| 値名                     | 設定画面の項目                     | 内容                                               | 初期値・制約          |
| ------------------------ | ---------------------------------- | -------------------------------------------------- | --------------------- |
| `AdServer`               | AD Server                          | Active Directoryサーバー名                         | 環境に応じて指定      |
| `CwfAddress`             | CWFAddress                         | ポートレット画面のURL                              | HTTPまたはHTTPS       |
| `IntervalMinutes`        | 確認間隔                           | 自動更新間隔（分）                                 | 15～360分、初期値15分 |
| `NotifyByBar`            | ポップアップせず通知バーでお知らせ | 案件発見時にメイン画面ではなくWindows通知を表示    | 初期値OFF             |
| `Shortcut`               | ショートカットキー                 | 表示・非表示を切り替えるキー                       | 初期値`F3`            |
| `SchemaVersion`          | ―                                  | 設定形式のバージョン                               | アプリが自動設定      |
| `LegacyMigrationVersion` | ―                                  | 旧設定の自動移行を一度だけ試したことを示すマーカー | 移行前に自動設定      |
| `AppVersion`             | ―                                  | 起動したアプリ自身のバージョン                     | 起動時に自動設定      |

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
設定画面でPWを空欄にすると同じIDの保存済みPWを維持し、IDを変更する場合はPWの
再入力が必要です。`UseSAMLAuth=1`ではID/PWを空欄にでき、IDも空欄なら資格情報を
変更しません。入力したID/PWは保存できますが、認証時は保存値ではなく`SAML/SAML`を
POSTします。レジストリに旧形式の`Id`があっても参照・変更しません。

### GPO管理用の設定

GPOで設定を配布する場合は、配布前に次のレジストリ値を設定します。

| 値名             | 種類        | 設定内容                                                                                                                   |
| ---------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------- |
| `CwfAddress`     | `REG_SZ`    | 実際のCreate!WebフローのポートレットURL                                                                                    |
| `AdServer`       | `REG_SZ`    | 通常認証で使用するAD Server。SAMLでは省略可能                                                                              |
| `UseSAMLAuth`    | `REG_DWORD` | `1`ならSAML認証、未設定または`0`なら通常認証。アプリの設定画面からは変更しません                                           |
| `SchemaVersion`  | `REG_DWORD` | `1` 固定。無い場合、一般のアプリ設定は未設定として扱われます                                                               |
| `LatestVersion`  | `REG_SZ`    | 最新バージョン（オプション、それより古いバージョンを起動するとメニューに「⬆ アップデートあり」と表示されます）             |
| `MinimumVersion` | `REG_SZ`    | 最低バージョン（オプション、それより古いバージョンを起動すると現在のバージョンと必要な最低バージョンを表示し、終了します） |

起動時の扱いは次のとおりです。

- `SchemaVersion=1`がない場合、一般設定は未設定として扱います。
- 通常認証でID/PWが未登録なら、設定画面を開いて利用者に入力を求めます。
- `UseSAMLAuth=1`ならID/PWがなくても設定画面を省略し、`SAML/SAML`をPOSTします。
- SAMLで設定画面を省略した場合、未登録の確認間隔、通知設定、ショートカットだけを
  既定値で補完し、既存値やGPO配布値は上書きしません。

## 旧Electron版からの移行

### 移行処理の実行条件

次の旧設定ファイルがあり、`LegacyMigrationVersion`が未登録の場合だけ、アプリ起動時に自動移行を
一度実行します。GPO配布等により一般の設定が既に存在する場合も、下記の優先順位で統合します。

```text
%APPDATA%\createwebflowchecker\config.json
```

### 一般設定の優先順位

| 項目                                                     | 移行時の扱い                                                        |
| -------------------------------------------------------- | ------------------------------------------------------------------- |
| `CwfAddress`                                             | 未設定の場合だけ移行                                                |
| `AdServer`、`IntervalMinutes`、`NotifyByBar`、`Shortcut` | 旧設定に値があれば移行し、空または欠落なら既存のRust版／GPO値を維持 |
| `UseSAMLAuth`                                            | レジストリ値だけを使用し、旧設定からは移行しない                    |

### IDとPWの移行

- IDとPWは、Windows資格情報マネージャーの`KashiharaCity.CwfChecker`へ1組で保存します。
- 同じIDの現行資格情報があれば、そのPWを維持します。
- 同じIDの現行資格情報がなければ、Electron `safeStorage`の`encpw`を復号します。
- 復号できず現行資格情報もなければ、IDを空PWで保存し、通常認証では設定画面からの
  PW再入力へフォールバックします。
- 別IDの現行資格情報があり、旧PWを復号できなかった場合は上書きしません。
- SAML環境でも旧ID/PWは資格情報へ保存できますが、認証時には`SAML/SAML`をPOSTします。

### 移行後と失敗時

移行を試す前に`LegacyMigrationVersion=1`を保存するため、成否を問わず再移行しません。
旧`config.json`は移行成功時も削除しません。
旧設定を移行できなかった場合は警告を表示し、有効なRust版設定または既定値で起動を続けます。

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
誤実行を防ぐため、exe、msi、bat、cmd、PowerShell、JavaScriptなどの
実行・スクリプト形式はダウンロードと自動起動の対象外です。
ただし、アプリ側の拡張子ブロックは追加防御であり、サーバー側の添付ポリシーを置き換えるものではありません。

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

## ライセンス

[MIT License](./LICENSE)
