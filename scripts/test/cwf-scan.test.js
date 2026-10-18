/* cwf-scan.jsを最小限のDOM代替の上で実行し、画像の応答が止まっても
 * 認証・案件の報告（document.titleへの合図）が届くことを検証する。 */
"use strict";

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");
const { runInNewContext } = require("node:vm");

const scriptsDirectory = join(__dirname, "..", "..", "src-tauri", "scripts");
const scanSource = readFileSync(join(scriptsDirectory, "cwf-scan.js"), "utf8");
const coreSource = readFileSync(join(scriptsDirectory, "cwf-scan-core.js"), "utf8");

const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

/* 認証成功・案件1件のポートレット文書を模した、スクリプトが触る分だけのDOM。
 * imagesComplete=false のとき、画像は読込み完了も失敗も通知しない。
 * imagesFound=true のとき、候補画像はすべて見つかり、フッターの挿入まで進む。
 * decodeSettles=false のとき、挿入した画像のdecode()は決して完了しない。 */
const runScan = async ({ imagesComplete, imageProbeTimeoutMs, imagesFound = false, decodeSettles = true }) => {
  const titles = [];
  const decisionLink = {
    getAttribute: name =>
      name === "href" ? "javascript:showInputForm('/XFV20/receive/spf/approve_form?x=1')" : null,
  };
  let title = "ワークフローシステム";
  const document = {
    readyState: "complete",
    body: { innerHTML: "<!-- 認証成功 --><ul class=\"form-list_h\"><span class=\"dummy\">1</span></ul>" },
    querySelectorAll: selector => (selector === "a[href]" ? [decisionLink] : []),
    querySelector: selector => {
      if (selector === "ul.form-list_h span.dummy") return { textContent: "1" };
      if (selector === "div.contents") return imagesFound ? contents : null;
      return null;
    },
    createElement: tag =>
      tag === "img"
        ? new Image()
        : { style: {}, className: "", appendChild: () => {}, getBoundingClientRect: () => ({ bottom: 120 }) },
    // 実際のブラウザーと同じく、未設定のstyle値は空文字列。
    documentElement: { style: { overflowY: "" } },
    get title() { return title; },
    set title(value) { title = value; titles.push(value); },
  };
  class Image {
    constructor() { this.style = {}; }
    set src(value) {
      if (!imagesComplete || !value) return;
      const handler = imagesFound ? "onload" : "onerror";
      setTimeout(() => this[handler] && this[handler](), 0);
    }
    get naturalWidth() { return imagesFound ? 100 : 0; }
    decode() { return decodeSettles ? Promise.resolve() : new Promise(() => {}); }
  }
  const footerElements = [];
  const contents = { insertAdjacentElement: (position, element) => footerElements.push(element) };
  const window = {
    name: "",
    scrollY: 0,
    location: { origin: "https://cwf.example", href: "https://cwf.example/XFV20/portlet/wfportlet.jsp" },
    sessionStorage: { getItem: () => null, setItem: () => {} },
    document,
    Image,
    setTimeout,
    clearTimeout,
    requestAnimationFrame: callback => setTimeout(callback, 0),
    console: { error: () => {} },
  };
  window.window = window;
  window.top = window;
  const config = {
    allowedOrigin: "https://cwf.example",
    generationPrefix: "__CWFCHECKER_LOAD__",
    generationStorageKey: "__cwfcheckerLoadGeneration",
    imageProbeTimeoutMs,
  };
  runInNewContext(`(${scanSource})(config, (${coreSource}));`, { ...window, config });
  return { window, document, titles, footerElements };
};

const reports = titles => titles.filter(value => value.startsWith("__CWFCHECKER_REPORT__|"));

test("reports authentication and decisions once images have answered", async () => {
  const { window, titles } = await runScan({ imagesComplete: true, imageProbeTimeoutMs: 10000 });
  await sleep(100);
  assert.deepEqual(reports(titles), ["__CWFCHECKER_REPORT__|1|1|1|0|0|1"]);
  assert.equal(window.__cwfScanRunning, false);
});

test("still reports when footer images never answer, after the probe timeout", async () => {
  const { window, titles } = await runScan({ imagesComplete: false, imageProbeTimeoutMs: 50 });
  await sleep(300);
  assert.deepEqual(reports(titles), ["__CWFCHECKER_REPORT__|1|1|1|0|0|1"]);
  assert.equal(window.__cwfScanRunning, false);
});

test("inserts the footer and still reports when the footer image never finishes decoding", async () => {
  const { window, document, titles, footerElements } = await runScan({
    imagesComplete: true, imagesFound: true, decodeSettles: false, imageProbeTimeoutMs: 50,
  });
  await sleep(400);
  // 候補10件が見つかり、挿入したフッターの高さ120で報告する。
  assert.deepEqual(reports(titles), ["__CWFCHECKER_REPORT__|1|1|1|10|120|1"]);
  assert.equal(footerElements.length, 1);
  assert.equal(window.__cwfScanRunning, false);
  // 高さを測るために止めたスクロールは、期限切れでも元へ戻す。
  assert.equal(document.documentElement.style.overflowY, "");
});

test("without the timeout, hanging images would block the report (regression guard)", async () => {
  const { window, titles } = await runScan({ imagesComplete: false, imageProbeTimeoutMs: 10000 });
  await sleep(300);
  assert.deepEqual(reports(titles), []);
  assert.equal(window.__cwfScanRunning, true);
});
