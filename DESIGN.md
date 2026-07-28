# CreateWebFlowChecker Tauri移植方針

- Windows 10/11 x64専用、npm・Node.js・JavaScriptパッケージ不使用。
- 配布物はWebView2 Runtimeを同梱しない単独EXEとする。
- 設定は `HKCU\Software\KashiharaCity\CwfChecker` に保存する。
- IDとパスワードはWindows資格情報 `KashiharaCity.CwfChecker` に保存する。
- SAML認証は管理者配布のDWORD値 `UseSAMLAuth=1` で有効にし、保存したID/PWとは分離する。
- レジストリ設定がなければ旧 `config.json` と旧資格情報 `cwfchecker/{ID}` を移行する。
- 移行は書き込み・読み戻し確認後に確定し、成功後だけ旧JSONと旧資格情報を削除する。
- WebView2のユーザーデータは一時ディレクトリに置き、次回起動時に削除する。
- 永続ログ、自動更新、TLS検証無効化は行わない。認証情報はURLやログへ出さず、
  公式ポートレット仕様のPOST本文で送信する。
- Cargo依存は公開後3日経過を確認し、`cargo audit` 後に `--locked` でビルドする。
