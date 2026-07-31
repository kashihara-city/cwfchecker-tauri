/* CWFポートレットの認証マーカー、案件リンク、通知用件数をDOMから抽出するファクトリ。
 * WebViewのcwf-scan.jsとNodeのfixtureテストの両方から使用する。 */
() => {
  const DECISION_PATH_PATTERN =
    /\/XFV20\/receive\/spf\/[^/?#'"()\s<>]+_form(?![^/?#'"()\s<>])/i;
  const AUTH_SUCCESS_MARKER = "<!-- 認証成功 -->";

  const countOccurrences = (source, needle) =>
    needle ? source.split(needle).length - 1 : 0;

  const isDecisionHref = href =>
    typeof href === "string" && DECISION_PATH_PATTERN.test(href);

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
      countText,
    };
  };

  return Object.freeze({
    AUTH_SUCCESS_MARKER,
    DECISION_PATH_PATTERN,
    isDecisionHref,
    scanCwfDocument,
  });
}
