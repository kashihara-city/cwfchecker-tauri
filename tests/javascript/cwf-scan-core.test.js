/* 実CWF画面から作ったHTML fixtureで、cwf-scan-core.jsのDOM判定を検証する。 */
"use strict";

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");
const { runInNewContext } = require("node:vm");

const corePath = join(__dirname, "..", "..", "src-tauri", "scripts", "cwf-scan-core.js");
const createCwfScanCore = runInNewContext(`(${readFileSync(corePath, "utf8")})`);
const { isDecisionHref, scanCwfDocument } = createCwfScanCore();

const fixtureDirectory = join(__dirname, "..", "fixtures");

const attribute = (attributes, name) => {
  const match = attributes.match(
    new RegExp(`\\b${name}\\s*=\\s*(["'])(.*?)\\1`, "is")
  );
  return match?.[2] ?? null;
};

const fixtureDocument = html => {
  const bodyHtml = html.match(/<body\b[^>]*>([\s\S]*)<\/body>/i)?.[1] ?? html;
  const anchors = [...bodyHtml.matchAll(/<a\b([^>]*)>/gis)]
    .map(match => attribute(match[1], "href"))
    .filter(href => href !== null)
    .map(href => ({ getAttribute: name => name === "href" ? href : null }));
  const countMatch = bodyHtml.match(
    /<ul\b[^>]*class=["'][^"']*\bform-list_h\b[^"']*["'][^>]*>[\s\S]*?<span\b[^>]*class=["'][^"']*\bdummy\b[^"']*["'][^>]*>([\s\S]*?)<\/span>/i
  );
  const countNode = countMatch
    ? { textContent: countMatch[1].replace(/<[^>]+>/g, "") }
    : null;

  return {
    body: { innerHTML: bodyHtml },
    querySelectorAll(selector) {
      assert.equal(selector, "a[href]");
      return anchors;
    },
    querySelector(selector) {
      assert.equal(selector, "ul.form-list_h span.dummy");
      return countNode;
    },
  };
};

const fixtureCases = [
  ["authenticated_one_decision.html", 1, 1, "1"],
  ["authenticated_no_decisions.html", 1, 0, "0"],
  ["authenticated_more_pending_than_rendered.html", 1, 5, "6"],
  ["authentication_failed.html", 0, 0, "0"],
];

for (const [name, authCount, decisionCount, countText] of fixtureCases) {
  test(`scans ${name}`, () => {
    const html = readFileSync(join(fixtureDirectory, name), "utf8");

    assert.deepEqual({ ...scanCwfDocument(fixtureDocument(html)) }, {
      decisionCount,
      authCount,
      countText,
    });
  });
}

test("ignores an authentication marker outside body", () => {
  const html = "<!-- 認証成功 --><html><body><p>認証画面</p></body></html>";

  assert.equal(scanCwfDocument(fixtureDocument(html)).authCount, 0);
});

test("recognizes direct and JavaScript decision links", () => {
  assert.equal(
    isDecisionHref("/XFV20/receive/spf/approve_form?fixture=direct"),
    true
  );
  assert.equal(
    isDecisionHref(
      "javascript:showInputForm('/XFV20/receive/spf/approve_form?fixture=script')"
    ),
    true
  );
  assert.equal(
    isDecisionHref("/XFV20/receive/spf/confirm_form?fixture=other-type"),
    true
  );
  assert.equal(isDecisionHref("/XFV20/receive/spf/approve_form/12345"), true);
  assert.equal(isDecisionHref("/XFV20/receive/spf/list"), false);
  assert.equal(isDecisionHref("/XFV20/receive/spf/foo_form_extra"), false);
  assert.equal(isDecisionHref("/XFV20/receive/spf/approve_form.do"), false);
  assert.equal(isDecisionHref("/XFV20/receive/other/foo_form"), false);
  assert.equal(isDecisionHref(null), false);
});
