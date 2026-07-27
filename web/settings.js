// Tauriがローカルページへ公開するinvokeだけを取り出す。
// パスワードを含む設定は、ネットワークではなくRust側のコマンドへ直接渡される。
const { invoke } = window.__TAURI__.core;

// 同じ要素を何度も検索せず、RustのSettingsInputと対応する名前でまとめておく。
const fields = {
  id: document.querySelector("#id"),
  password: document.querySelector("#password"),
  adServer: document.querySelector("#ad-server"),
  cwfAddress: document.querySelector("#cwf-address"),
  intervalMinutes: document.querySelector("#interval"),
  notifyByBar: document.querySelector("#notify"),
  shortcut: document.querySelector("#shortcut"),
};

const status = document.querySelector("#status");
const version = document.querySelector("#app-version");
const form = document.querySelector("#settings-form");
const submitButton = form.querySelector('button[type="submit"]');

// メッセージはinnerHTMLではなくtextContentへ入れ、エラー文字列をHTMLとして
// 解釈させない。
function showStatus(message, error = false) {
  status.textContent = message;
  status.classList.toggle("error", error);
}

async function load() {
  try {
    // get_settingsはPWそのものを返さない。PW欄は常に空欄から始める。
    const settings = await invoke("get_settings");
    fields.id.value = settings.id ?? "";
    fields.adServer.value = settings.adServer ?? "";
    fields.cwfAddress.value = settings.cwfAddress ?? "";
    fields.intervalMinutes.value = settings.intervalMinutes ?? 15;
    fields.notifyByBar.checked = Boolean(settings.notifyByBar);
    fields.shortcut.value = settings.shortcut || "F3";
    version.textContent = settings.version || "—";
  } catch (error) {
    showStatus(String(error), true);
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  showStatus("保存しています…");
  // ダブルクリックによる同時保存と、複数の再起動予約を防ぐ。
  submitButton.disabled = true;
  try {
    await invoke("save_settings", {
      input: {
        id: fields.id.value,
        password: fields.password.value,
        adServer: fields.adServer.value,
        cwfAddress: fields.cwfAddress.value,
        intervalMinutes: Number(fields.intervalMinutes.value),
        notifyByBar: fields.notifyByBar.checked,
        shortcut: fields.shortcut.value,
      },
    });
    showStatus("保存しました。アプリを再起動します。");
  } catch (error) {
    showStatus(String(error), true);
    submitButton.disabled = false;
  }
});

// DOM末尾で読み込まれるスクリプトなので、DOMContentLoaded待ちは不要。
load();
