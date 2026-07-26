(config) => {
  if (window.location.href !== config.bootstrapUrl) return;

  // POST後に作られるCWF文書へ、この読込み世代をURLやPOST本文へ載せず引き継ぐ。
  window.name = config.windowName;
  const form = document.createElement("form");
  form.method = "post";
  form.action = config.action;
  form.referrerPolicy = "no-referrer";
  for (const [name, value] of config.fields) {
    const input = document.createElement("input");
    input.type = "hidden";
    input.name = name;
    input.value = value;
    form.appendChild(input);
  }
  document.body.appendChild(form);
  form.submit();
}
