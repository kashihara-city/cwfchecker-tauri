# CreateWebFlowChecker Tauri移植方針

- Windows 10/11 x64専用、npm・Node.js・JavaScriptパッケージ不使用。
- 配布物はWebView2 Runtimeを同梱しない単独EXEとする。
- 設定は `HKCU\Software\KashiharaCity\CwfChecker` に保存する。
- IDとパスワードはWindows資格情報 `KashiharaCity.CwfChecker` に保存する。
- SAML認証は管理者配布のDWORD値 `UseSAMLAuth=1` で有効にし、保存したID/PWとは分離する。
- 移行済みマーカーがなければ旧 `config.json` を使った自動移行を一度だけ試す。
- 移行前にマーカーを保存し、成否にかかわらず再試行しない。旧JSONは削除しない。
- WebView2のユーザーデータは一時ディレクトリに置き、次回起動時に削除する。
- 永続ログ、自動更新、TLS検証無効化は行わない。認証情報はURLやログへ出さず、
  公式ポートレット仕様のPOST本文で送信する。
- Cargo依存は公開後14日経過を確認し、`cargo audit` 後に `--locked` でビルドする。
- JavaScriptはActionsで構文・単体テストを行い、実画面由来の4 HTML fixtureでDOM判定を検証する。
- `withGlobalTauri` はIPCの認可設定ではなく、公開JavaScript API一式を
  `window.__TAURI__` に載せる設定である。無効でもTauri 2.11.5のWindows実装では
  `window.__TAURI_INTERNALS__` とIPC機構が各documentへ注入され、CWFへの遷移後も
  利用可能な形で存在する。ただし`window.__TAURI_INTERNALS__`は公開APIではなく、
  長期互換性を前提にできないため、これを設定画面から直接呼ぶことで
  `withGlobalTauri`を無効化しない。npm・JavaScriptパッケージを使わない方針のもと、
  設定画面は公開APIの`window.__TAURI__`を使うため`withGlobalTauri`を有効にする。
  CWFのリモートoriginにはremote capabilityを付与しないため、IPC要求はRustコマンドへ
  到達する前にRuntime Authorityが拒否する。設定コマンドでもウィンドウラベルを検査し、
  この認可に依存しない多層防御を維持する。
