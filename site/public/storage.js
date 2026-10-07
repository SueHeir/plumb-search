// Shows how much data the node on this server searches, from its own
// /api/status, and refreshes it every 30 seconds. Nothing is sent anywhere
// else, and nothing about the visitor is kept.
(function () {
  "use strict";
  var box = document.getElementById("storage");
  var used = document.getElementById("storage-used");
  var limit = document.getElementById("storage-limit");
  if (!box || !used || !limit || !window.fetch) return;

  // Gigabytes of 1,024 of the node's megabytes (10^6 bytes each), so a
  // storage limit set as 65,536 MB reads as the 64 GB it was chosen as.
  function gb(bytes) {
    var n = bytes / 1.024e9;
    return (n >= 100 ? Math.round(n) : n.toFixed(1)) + " GB";
  }

  function update() {
    if (document.hidden) return;
    fetch("/api/status", { cache: "no-store" })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (s) {
        if (!s || s.phase !== "ready" || !s.disk_used) {
          box.hidden = true;
          return;
        }
        used.textContent = gb(s.disk_used);
        limit.textContent = s.storage_limit ? ", of a " + gb(s.storage_limit).replace(".0 GB", " GB") + " limit" : "";
        box.hidden = false;
      })
      .catch(function () { box.hidden = true; });
  }

  update();
  setInterval(update, 30000);
  document.addEventListener("visibilitychange", update);
})();
