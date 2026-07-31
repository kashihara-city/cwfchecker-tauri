/* CWFポートレットの認証マーカー、案件リンク、通知用件数をDOMから抽出する共有処理。
 * WebViewのcwf-scan.jsとNodeのfixtureテストの両方から使用する。 */
(root => {
  const DECISION_PATH = "/XFV20/receive/spf/approve_form";
  const AUTH_SUCCESS_MARKER = "<!-- 認証成功 -->";
  const AUTH_FAILURE_MARKER = "<!-- 認証失敗 -->";

  const countOccurrences = (source, needle) =>
    needle ? source.split(needle).length - 1 : 0;

  const isDecisionHref = href =>
    typeof href === "string" && href.includes(DECISION_PATH);

  const scanCwfDocument = document => {
    const html = document.body?.innerHTML || "";
    const decisionCount = [...document.querySelectorAll("a[href]")]
      .filter(anchor => isDecisionHref(anchor.getAttribute("href")))
      .length;
    const countText = document.querySelector("ul.form-list_h span.dummy")
      ?.textContent?.trim() || "0";

    return {
      decisionCount,
      authCount: countOccurrences(html, AUTH_SUCCESS_MARKER),
      authFailureCount: countOccurrences(html, AUTH_FAILURE_MARKER),
      countText,
    };
  };

  const api = Object.freeze({
    AUTH_FAILURE_MARKER,
    AUTH_SUCCESS_MARKER,
    DECISION_PATH,
    isDecisionHref,
    scanCwfDocument,
  });

  root.__cwfScanCore = api;
  if (typeof module === "object" && module.exports) module.exports = api;
})(typeof globalThis === "object" ? globalThis : this);
