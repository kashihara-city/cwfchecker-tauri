/* 案件ウィンドウのページが呼ぶwindow.close()を、Rust側のウィンドウ終了へ置き換える。
 * WebView2でそのまま実行するとWebViewの入れ物だけが壊れ、枠が黒いまま残るため、
 * document.titleで合図を送り、Rustが通常の閉じる処理を行う。 */
(config) => {
  // iframe内のwindow.close()はもともと何もしないので、最上位の文書だけ差し替える。
  if (window.top !== window) return;
  const requestClose = () => {
    // CWFは閉じた直後に同じ名前でwindow.open()する（一覧の案件リンク）。Rustが
    // 閉じ終わる前に同名の窓として再利用されないよう、名前を先に外しておく。
    window.name = "";
    // Rustが閉じるまでの間に再度呼ばれても、同じ合図を出すだけで害はない。
    document.title = config.closeTitle;
  };
  // 自分のページからも、開いた側（opener）からの winDetail.close() からも、
  // この関数が呼ばれる。
  window.close = requestClose;
}
