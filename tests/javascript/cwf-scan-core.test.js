/* 実CWF画面から作ったHTML fixtureで、cwf-scan-core.jsのDOM判定を検証する。 */
"use strict";

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");

const {
  isDecisionHref,
  scanCwfDocument,
} = require("../../src-tauri/scripts/cwf-scan-core.js");

const fixtureDirectory = join(__dirname, "..", "fixtures");

const attribute = (attributes, name) => {
  const match = attributes.match(
    new RegExp(`\\b${name}\\s*=\\s*(["'])(.*?)\\1`, "is")
  );
  return match?.[2] ?? null;
};

const fixtureDocument = html => {
  const anchors = [...html.matchAll(/<a\b([^>]*)>/gis)]
    .map(match => attribute(match[1], "href"))
    .filter(href => href !== null)
    .map(href => ({ getAttribute: name => name === "href" ? href : null }));
  const countMatch = html.match(
    /<ul\b[^>]*class=["'][^"']*\bform-list_h\b[^"']*["'][^>]*>[\s\S]*?<span\b[^>]*class=["'][^"']*\bdummy\b[^"']*["'][^>]*>([\s\S]*?)<\/span>/i
  );
  const countNode = countMatch
    ? { textContent: countMatch[1].replace(/<[^>]+>/g, "") }
    : null;

  return {
    body: { innerHTML: html },
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
  ["authenticated_one_decision.html", 1, 0, 1, "1"],
  ["authenticated_no_decisions.html", 1, 0, 0, "0"],
  ["authenticated_more_pending_than_rendered.html", 1, 0, 5, "6"],
  ["authentication_failed.html", 0, 1, 0, "0"],
];

for (const [name, authCount, authFailureCount, decisionCount, countText]
  of fixtureCases) {
  test(`scans ${name}`, () => {
    const html = readFileSync(join(fixtureDirectory, name), "utf8");

    assert.deepEqual(scanCwfDocument(fixtureDocument(html)), {
      decisionCount,
      authCount,
      authFailureCount,
      countText,
    });
  });
}

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
  assert.equal(isDecisionHref("/XFV20/receive/spf/list"), false);
  assert.equal(isDecisionHref(null), false);
});
