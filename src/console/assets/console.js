/* The operator console: no framework, no build step, no network calls except
 * same-origin fetches for actions and partial refreshes.
 *
 * Conventions the server renders against:
 *   - any element with data-theme-toggle flips light/dark and remembers it
 *   - any element with data-lang navigates to its href and remembers the language
 *   - form[data-endpoint] is a console mutation: serialised to JSON, POSTed with
 *     the X-LGW-Console header, and the response HTML replaces the element
 *     named by data-result
 *   - form[data-confirm] asks first using the page's <dialog data-confirm-dialog>
 *   - button[data-submit-form] triggers its associated form; button[data-validate]
 *     sends the form to data-validate-url and renders the answer only
 *   - [data-poll] refreshes from data-url every data-interval ms while visible
 */
(function () {
  "use strict";

  var root = document.documentElement;
  root.setAttribute("data-js", "on");

  var THEME_KEY = "lgw-theme";
  var LANG_KEY = "lgw-lang";

  function store(key, value) {
    try { window.localStorage.setItem(key, value); } catch (e) { /* ignore */ }
  }

  function stored(key) {
    try { return window.localStorage.getItem(key); } catch (e) { return null; }
  }

  /* ------------------------------------------------------------- theme */

  function currentTheme() {
    return root.getAttribute("data-theme") === "dark" ? "dark" : "light";
  }

  function applyTheme(theme) {
    root.setAttribute("data-theme", theme);
    var buttons = document.querySelectorAll("[data-theme-toggle]");
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].setAttribute("aria-pressed", theme === "dark" ? "true" : "false");
    }
    store(THEME_KEY, theme);
  }

  var themeButtons = document.querySelectorAll("[data-theme-toggle]");
  for (var t = 0; t < themeButtons.length; t++) {
    themeButtons[t].addEventListener("click", function () {
      applyTheme(currentTheme() === "dark" ? "light" : "dark");
    });
  }
  applyTheme(currentTheme());

  /* ------------------------------------------------------------ language */

  var langLinks = document.querySelectorAll("[data-lang]");
  for (var l = 0; l < langLinks.length; l++) {
    langLinks[l].addEventListener("click", function () {
      store(LANG_KEY, this.getAttribute("data-lang"));
    });
  }

  (function rememberLanguage() {
    var url = new URL(window.location.href);
    if (url.searchParams.has("lang")) {
      return;
    }
    var remembered = stored(LANG_KEY);
    if (!remembered || remembered === document.documentElement.lang) {
      return;
    }
    url.searchParams.set("lang", remembered);
    window.location.replace(url.toString());
  })();

  /* --------------------------------------------------------------- fetch */

  function headers() {
    return {
      "Content-Type": "application/json",
      "Accept": "text/html",
      "X-LGW-Console": "1"
    };
  }

  function serialize(form) {
    var out = {};
    var data = new FormData(form);
    data.forEach(function (value, key) {
      out[key] = value;
    });
    var boxes = form.querySelectorAll('input[type="checkbox"][name]');
    for (var i = 0; i < boxes.length; i++) {
      out[boxes[i].name] = boxes[i].checked ? "true" : "false";
    }
    return out;
  }

  function resultTarget(form) {
    var selector = form.getAttribute("data-result");
    if (!selector) { return null; }
    return document.querySelector(selector);
  }

  function showFlash(message, kind) {
    var old = document.querySelector(".flash");
    if (old) { old.remove(); }
    var node = document.createElement("div");
    node.className = "flash" + (kind ? " " + kind : "");
    node.setAttribute("role", "status");
    node.textContent = message;
    document.body.appendChild(node);
    window.setTimeout(function () { node.remove(); }, 6000);
  }

  function busy(form, on) {
    form.setAttribute("aria-busy", on ? "true" : "false");
    var buttons = form.querySelectorAll("button");
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].disabled = on;
    }
  }

  async function send(form, url, payload) {
    busy(form, true);
    try {
      var response = await fetch(url, {
        method: "POST",
        headers: headers(),
        body: JSON.stringify(payload),
        credentials: "same-origin",
        redirect: "follow"
      });
      var text = await response.text();
      var target = resultTarget(form);
      if (target) {
        target.innerHTML = text;
      }
      return { status: response.status, text: text };
    } catch (error) {
      showFlash(String(error), "error");
      return { status: 0, text: "" };
    } finally {
      busy(form, false);
    }
  }

  /* --------------------------------------------------------------- forms */

  var confirmDialog = document.querySelector("dialog[data-confirm-dialog]");
  var pendingForm = null;
  var pendingSubmit = null;

  function ask(form) {
    if (!confirmDialog) { return true; }
    if (!form.getAttribute("data-confirm")) { return true; }
    var message = confirmDialog.querySelector("[data-confirm-message]");
    if (message) { message.textContent = form.getAttribute("data-confirm"); }
    pendingForm = form;
    confirmDialog.showModal();
    return false;
  }

  if (confirmDialog) {
    var confirmButton = confirmDialog.querySelector("[data-confirm-accept]");
    var cancelButton = confirmDialog.querySelector("[data-confirm-cancel]");
    if (confirmButton) {
      confirmButton.addEventListener("click", function () {
        confirmDialog.close("confirm");
      });
    }
    if (cancelButton) {
      cancelButton.addEventListener("click", function () {
        confirmDialog.close("cancel");
      });
    }
    confirmDialog.addEventListener("close", function () {
      var form = pendingForm;
      pendingForm = null;
      if (confirmDialog.returnValue === "confirm" && form) {
        form.setAttribute("data-confirmed", "true");
        form.requestSubmit();
      }
    });
  }

  var jsonForms = document.querySelectorAll("form[data-endpoint]");
  for (var f = 0; f < jsonForms.length; f++) {
    (function (form) {
      form.addEventListener("submit", function (event) {
        event.preventDefault();
        if (form.getAttribute("data-confirm") && form.getAttribute("data-confirmed") !== "true") {
          if (!ask(form)) { return; }
          return;
        }
        form.removeAttribute("data-confirmed");
        var url = form.getAttribute("data-endpoint") || form.action;
        send(form, url, serialize(form)).then(function (result) {
          if (result.status === 200) {
            form.setAttribute("data-dirty", "false");
          }
        });
      });
    })(jsonForms[f]);
  }

  var validateButtons = document.querySelectorAll("[data-validate]");
  for (var v = 0; v < validateButtons.length; v++) {
    (function (button) {
      button.addEventListener("click", function () {
        var form = document.getElementById(button.getAttribute("data-submit-form"));
        if (!form) { return; }
        var url = form.getAttribute("data-validate-url");
        if (!url) { return; }
        send(form, url, serialize(form));
      });
    })(validateButtons[v]);
  }

  var submitButtons = document.querySelectorAll("[data-submit-form]");
  for (var b = 0; b < submitButtons.length; b++) {
    (function (button) {
      if (button.hasAttribute("data-validate")) { return; }
      button.addEventListener("click", function () {
        var form = document.getElementById(button.getAttribute("data-submit-form"));
        if (form) { form.requestSubmit(); }
      });
    })(submitButtons[b]);
  }

  /* dirty tracking: the apply bar only wakes up when something changed */
  var dirtyForms = document.querySelectorAll("form[data-dirty-watch]");
  for (var d = 0; d < dirtyForms.length; d++) {
    (function (form) {
      var initial = JSON.stringify(serialize(form));
      function refresh() {
        var dirty = JSON.stringify(serialize(form)) !== initial;
        form.setAttribute("data-dirty", dirty ? "true" : "false");
        var gates = document.querySelectorAll("[data-requires-dirty]");
        for (var g = 0; g < gates.length; g++) {
          gates[g].disabled = !dirty;
        }
        var label = document.querySelector("[data-dirty-label]");
        if (label) { label.hidden = !dirty; }
      }
      form.addEventListener("input", refresh);
      form.addEventListener("change", refresh);
      refresh();
    })(dirtyForms[d]);
  }

  /* --------------------------------------------------------------- polls */

  var polls = document.querySelectorAll("[data-poll]");
  for (var p = 0; p < polls.length; p++) {
    (function (node) {
      var interval = Number(node.getAttribute("data-interval")) || 10000;
      var failures = 0;
      var timer = null;

      async function refresh() {
        if (document.hidden) { return; }
        try {
          var response = await fetch(node.getAttribute("data-url"), {
            headers: { "Accept": "text/html" },
            credentials: "same-origin"
          });
          if (!response.ok) { throw new Error("HTTP " + response.status); }
          node.innerHTML = await response.text();
          failures = 0;
          node.removeAttribute("data-stale");
          var stamp = node.querySelector("[data-updated]");
          if (stamp) {
            stamp.textContent = new Date().toLocaleTimeString();
          }
        } catch (error) {
          failures += 1;
          node.setAttribute("data-stale", "true");
          if (failures >= 3) {
            window.clearInterval(timer);
            timer = null;
          }
        }
      }

      function schedule() {
        if (timer) { window.clearInterval(timer); }
        timer = window.setInterval(refresh, interval);
      }

      // The refresh button finds its poller through this property, so it keeps
      // working after the fragment (and the button) has been replaced.
      node.lgwRefresh = refresh;
      var firstStamp = node.querySelector("[data-updated]");
      if (firstStamp) {
        firstStamp.textContent = new Date().toLocaleTimeString();
      }

      document.addEventListener("visibilitychange", function () {
        if (!document.hidden) { refresh(); }
      });
      schedule();
    })(polls[p]);
  }

  var refreshButtons = document.querySelectorAll("[data-refresh]");
  for (var rb = 0; rb < refreshButtons.length; rb++) {
    refreshButtons[rb].addEventListener("click", function () {
      var nodes = document.querySelectorAll("[data-poll]");
      for (var n = 0; n < nodes.length; n++) {
        if (nodes[n].lgwRefresh) { nodes[n].lgwRefresh(); }
      }
    });
  }


  /* -------------------------------------------- configuration drafts */

  // The configuration form posts the changed leaves, never the whole file:
  // each entry carries the TOML path as segments, so a model name with a dot or
  // a slash is never split by a string parser on either side.
  var draftForms = document.querySelectorAll("form[data-config-draft]");
  for (var df = 0; df < draftForms.length; df++) {
    (function (form) {
      var validateButton = form.querySelector("[data-config-validate]");
      var applyButton = form.querySelector("[data-config-apply]");
      var result = document.querySelector("[data-config-result]");
      // Only real controls: the remove buttons also carry a data-path, and
      // including them here sent the account table itself as a leaf write.
      var fields = form.querySelectorAll("input[data-path], select[data-path]");

      function draft() {
        var entries = [];

        // Changed leaves only: an untouched default that the file omits must
        // not be sent as a write.
        for (var i = 0; i < fields.length; i++) {
          var field = fields[i];
          if (field.value === field.getAttribute("data-initial")) {
            continue;
          }
          entries.push({
            action: "set",
            path: JSON.parse(field.getAttribute("data-path")),
            value: field.value
          });
        }

        // Whole-account edits: a removals list and, per provider, an add form
        // that only participates once its id is filled in.
        var removers = form.querySelectorAll("[data-account-remove].is-armed");
        for (var r = 0; r < removers.length; r++) {
          entries.push({
            action: "remove",
            path: JSON.parse(removers[r].getAttribute("data-path"))
          });
        }
        var entityRemovers = form.querySelectorAll("[data-remove-action].is-armed");
        for (var er = 0; er < entityRemovers.length; er++) {
          entries.push(Object.assign(
            { action: entityRemovers[er].getAttribute("data-remove-action") },
            JSON.parse(entityRemovers[er].getAttribute("data-payload"))
          ));
        }
        var addForms = form.querySelectorAll("[data-add-account]");
        for (var a = 0; a < addForms.length; a++) {
          var details = addForms[a];
          var idInput = details.querySelector('[data-add-field="id"]');
          var id = idInput ? idInput.value.trim() : "";
          if (!id) { continue; }
          var value = details.querySelector('[data-add-field="source_value"]');
          entries.push({
            action: "add_account",
            provider: details.getAttribute("data-provider"),
            id: id,
            label: details.querySelector('[data-add-field="label"]').value.trim(),
            key_source: details.querySelector('[data-add-field="source"]').value,
            key_value: value ? value.value.trim() : ""
          });
        }

        // Routes, aliases, prices and schedules share one add-form shape: a
        // fixed part names where the row goes, the fields name the values.
        var entityForms = form.querySelectorAll("[data-add-form]");
        for (var e = 0; e < entityForms.length; e++) {
          var box = entityForms[e];
          var entry = JSON.parse(box.getAttribute("data-static") || "{}");
          var required = box.getAttribute("data-required");
          var inputs = box.querySelectorAll("[data-add-field]");
          var filled = {};
          for (var i2 = 0; i2 < inputs.length; i2++) {
            var name = inputs[i2].getAttribute("data-add-field");
            var text = inputs[i2].value.trim();
            if (!text) { continue; }
            filled[name] = (name === "input" || name === "output" || name === "cache_read")
              ? Number(text)
              : text;
          }
          if (!filled[required]) { continue; }
          entries.push(Object.assign({ action: box.getAttribute("data-kind") }, entry, filled));
        }
        return entries;
      }

      function remember(field) {
        field.setAttribute("data-initial", field.value);
        var row = field.closest(".field");
        if (row) { row.classList.remove("changed"); }
      }

      async function send(button) {
        if (!button || !result) { return; }
        var buttons = [validateButton, applyButton];
        for (var b = 0; b < buttons.length; b++) {
          if (buttons[b]) { buttons[b].disabled = true; }
        }
        result.setAttribute("aria-busy", "true");
        try {
          var url = button.hasAttribute("data-validate-url")
            ? button.getAttribute("data-validate-url")
            : button.getAttribute("data-apply-url");
          var response = await fetch(url, {
            method: "POST",
            headers: headers(),
            credentials: "same-origin",
            body: JSON.stringify({
              fingerprint: form.getAttribute("data-fingerprint"),
              draft: draft()
            })
          });
          result.innerHTML = await response.text();
          result.classList.toggle("is-error", !response.ok);
          var applied = result.querySelector("[data-applied]");
          if (applied) {
            form.setAttribute("data-fingerprint", applied.getAttribute("data-fingerprint"));
            for (var f = 0; f < fields.length; f++) { remember(fields[f]); }
            if (applied.hasAttribute("data-reload")) {
              // A structural apply changes the rows themselves; the page has to
              // be rebuilt from the new file rather than patched in place.
              window.setTimeout(function () { window.location.reload(); }, 700);
            }
          }
        } catch (error) {
          result.textContent = String(error);
          result.classList.add("is-error");
        } finally {
          for (var b2 = 0; b2 < buttons.length; b2++) {
            if (buttons[b2]) { buttons[b2].disabled = false; }
          }
          result.removeAttribute("aria-busy");
        }
      }

      if (validateButton) {
        validateButton.addEventListener("click", function () { send(validateButton); });
      }
      if (applyButton) {
        applyButton.addEventListener("click", function () {
          var question = applyButton.getAttribute("data-confirm");
          if (question && !window.confirm(question)) { return; }
          send(applyButton);
        });
      }
      var removers = form.querySelectorAll("[data-account-remove], [data-remove-action]");
      for (var r = 0; r < removers.length; r++) {
        (function (button) {
          button.addEventListener("click", function () {
            var armed = button.classList.toggle("is-armed");
            button.textContent = armed
              ? button.getAttribute("data-undo-label")
              : button.getAttribute("data-remove-label");
          });
        })(removers[r]);
      }

      form.addEventListener("submit", function (event) {
        event.preventDefault();
        send(validateButton);
      });

      for (var f = 0; f < fields.length; f++) {
        (function (field) {
          function mark() {
            var row = field.closest(".field");
            if (row) {
              row.classList.toggle(
                "changed",
                field.value !== field.getAttribute("data-initial")
              );
            }
          }
          field.addEventListener("input", mark);
          field.addEventListener("change", mark);
        })(fields[f]);
      }
    })(draftForms[df]);
  }

  /* --------------------------------------------------- forgetting prices */

  // A scoped forget is a guarded mutation: JSON plus the console header, which
  // headers() already supplies. The page reloads on success so the configured
  // price is visibly back in force.
  var forgetButtons = document.querySelectorAll("[data-forget]");
  for (var fb = 0; fb < forgetButtons.length; fb++) {
    (function (button) {
      button.addEventListener("click", async function () {
        var question = button.getAttribute("data-confirm");
        if (question && !window.confirm(question)) { return; }
        var payload = {};
        var provider = button.getAttribute("data-provider");
        var model = button.getAttribute("data-model");
        if (provider) { payload.provider = provider; }
        if (model) { payload.model = model; }
        button.disabled = true;
        try {
          var response = await fetch("/console/api/prices/forget", {
            method: "POST",
            headers: headers(),
            credentials: "same-origin",
            body: JSON.stringify(payload)
          });
          if (response.ok) {
            window.location.reload();
            return;
          }
          var result = document.querySelector("[data-price-result]");
          if (result) { result.textContent = await response.text(); }
        } catch (error) {
          var failed = document.querySelector("[data-price-result]");
          if (failed) { failed.textContent = String(error); }
        } finally {
          button.disabled = false;
        }
      });
    })(forgetButtons[fb]);
  }

  /* ------------------------------------------- board account actions */

  var accountButtons = document.querySelectorAll(
    "[data-account-probe], [data-account-disable], [data-account-enable]"
  );
  for (var ab = 0; ab < accountButtons.length; ab++) {
    (function (button) {
      button.addEventListener("click", async function () {
        var question = button.getAttribute("data-confirm");
        if (question && !window.confirm(question)) { return; }
        var provider = button.getAttribute("data-provider");
        var account = button.getAttribute("data-account");
        var probe = button.hasAttribute("data-account-probe");
        var payload = { provider: provider, account: account };
        if (!probe) {
          payload.enabled = button.hasAttribute("data-account-enable");
        }
        var row = button.closest(".row-actions");
        var siblings = row ? row.querySelectorAll("button") : [button];
        var page = document.querySelector("[data-board-result]");
        var label = button.textContent;
        button.textContent = "…";
        for (var d = 0; d < siblings.length; d++) { siblings[d].disabled = true; }
        try {
          var response = await fetch(
            probe ? "/console/api/accounts/probe" : "/console/api/accounts/state",
            {
              method: "POST",
              headers: headers(),
              credentials: "same-origin",
              body: JSON.stringify(payload)
            }
          );
          if (response.ok) {
            window.location.reload();
            return;
          }
          var detail = await response.text();
          try { detail = JSON.parse(detail).error || detail; } catch (ignored) {}
          if (page) { page.textContent = detail; }
        } catch (error) {
          if (page) { page.textContent = String(error); }
        } finally {
          button.textContent = label;
          for (var e = 0; e < siblings.length; e++) { siblings[e].disabled = false; }
        }
      });
    })(accountButtons[ab]);
  }

  /* flash messages vanish on their own; a close chip can remove them early */
  var flashes = document.querySelectorAll(".flash .close");
  for (var s = 0; s < flashes.length; s++) {
    flashes[s].addEventListener("click", function () {
      var flash = this.closest(".flash");
      if (flash) { flash.remove(); }
    });
  }
})();
