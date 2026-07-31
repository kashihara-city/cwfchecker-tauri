/* CWFページへ初期化時に注入し、DOM調査、フッター追加、Rustへの結果報告を行う。
 * Rust側が生成したconfigとcwf-scan-core.jsを前提にWebView内で実行される。 */
(config) => {
  // iframeや想定外サーバーでは、ページ内容の読取りやRust側への報告を行わない。
  if (window.top !== window || window.location.origin !== config.allowedOrigin) return;

  // 文書生成時の世代を固定する。次のreloadがwindow.nameを書き換えた後に
  // この文書の非同期スキャンが完了しても、新しい世代を誤って名乗らない。
  const windowNameGeneration = window.name.startsWith(config.generationPrefix)
    ? window.name.slice(config.generationPrefix.length)
    : "";
  let storedGeneration = "";
  // sessionStorageを使えない特殊なページでも、DOM調査そのものは止めない。
  try { storedGeneration = sessionStorage.getItem(config.generationStorageKey) || ""; } catch {}
  const isGeneration = value => /^\d+$/.test(value);
  // 初回POSTでは保存値がまだないため、WebView生成時の世代1を既定値にする。
  const loadGeneration = isGeneration(windowNameGeneration)
    ? windowNameGeneration
    : (isGeneration(storedGeneration) ? storedGeneration : "1");
  try { sessionStorage.setItem(config.generationStorageKey, loadGeneration); } catch {}

  const imageCandidates = [
    "cwfchecker_footer01.jpg", "cwfchecker_footer02.jpg",
    "cwfchecker_footer03.jpg", "cwfchecker_footer04.jpg",
    "cwfchecker_footer05.jpg", "cwfchecker_footer01.png",
    "cwfchecker_footer02.png", "cwfchecker_footer03.png",
    "cwfchecker_footer04.png", "cwfchecker_footer05.png"
  ];
  const checkImage = (url) => new Promise((resolve) => {
    const image = new Image();
    // ポートレットURLの認証クエリを画像リクエストのRefererへ載せない。
    image.referrerPolicy = "no-referrer";
    image.onload = image.onerror = () => resolve(image.naturalWidth > 0 ? url : null);
    image.src = url;
  });

  window.__cwfScan = async () => {
    // DOMContentLoadedとTauriのページ読込み完了が続けて発火しても、
    // 同じドキュメントの調査結果は一度だけRust側へ報告する。
    if (!document.body || window.__cwfScanRunning || window.__cwfScanReported) return;
    window.__cwfScanRunning = true;
    try {
      // 実際の承認フォームへのリンクを数え、描画行数と案件の有無を判定する。
      const { decisionCount, authCount, countText } =
        window.__cwfScanCore.scanCwfDocument(document);
      const baseUrl = window.location.href.split("XFV20")[0];
      const images = (await Promise.all(imageCandidates.map(
        name => checkImage(`${baseUrl}XFV20/manual/user/_images/${name}`)
      ))).filter(Boolean);

      // 更新のたびにフッターが増えないよう、前回追加した要素を先に取り除く。
      document.querySelectorAll(".cwfchecker-tauri-footer").forEach(node => node.remove());
      let contentHeight = 0;
      if (images.length > 0) {
        const footer = document.createElement("div");
        footer.className = "footer cwfchecker-tauri-footer";
        footer.style.width = "100%";
        footer.style.maxWidth = "100%";
        footer.style.boxSizing = "border-box";
        footer.style.overflow = "hidden";
        const image = document.createElement("img");
        image.referrerPolicy = "no-referrer";
        image.style.display = "block";
        image.style.width = "100%";
        image.style.maxWidth = "100%";
        image.style.height = "auto";
        image.style.objectFit = "contain";
        image.src = images[Math.floor(Math.random() * images.length)];
        footer.appendChild(image);
        const contents = document.querySelector("div.contents");
        if (contents) {
          contents.insertAdjacentElement("afterend", footer);
          const root = document.documentElement;
          const previousOverflowY = root.style.overflowY;
          root.style.overflowY = "hidden";
          try {
            try { await image.decode(); } catch {}
            // 画像のレイアウト確定を2フレーム待ってから、Rust側へ正確な高さを返す。
            await new Promise(resolve => requestAnimationFrame(
              () => requestAnimationFrame(resolve)
            ));
            contentHeight = Math.ceil(footer.getBoundingClientRect().bottom + window.scrollY);
          } finally {
            root.style.overflowY = previousOverflowY;
          }
        }
      }

      const previousTitle = document.title;
      // 外部ページにはTauri IPCを公開しないため、一時的なdocument.titleを
      // 最小限の通信路として使う。Rust側でもウィンドウ名とoriginを再検証する。
      window.__cwfScanReported = true;
      document.title = `__CWFCHECKER_REPORT__|${loadGeneration}|${decisionCount}|${authCount}|${images.length}|${contentHeight}|${countText.replaceAll("|", "")}`;
      await new Promise(resolve => setTimeout(resolve, 0));
      document.title = previousTitle;
    } finally {
      window.__cwfScanRunning = false;
    }
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => window.__cwfScan(), { once: true });
  } else {
    window.__cwfScan();
  }
}
