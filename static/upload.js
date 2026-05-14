// Progressive uploader: posts one file per request so every file gets its own
// progress bar. Falls back to a normal multipart form POST when JS is disabled.
(function () {
  "use strict";

  var list = document.getElementById("upload-progress");
  if (!list) return;

  function makeRow(name) {
    var li = document.createElement("li");
    li.className = "upload-row";

    var label = document.createElement("span");
    label.className = "upload-name";
    label.textContent = name;

    var bar = document.createElement("span");
    bar.className = "upload-bar";
    var fill = document.createElement("span");
    fill.className = "upload-fill";
    bar.appendChild(fill);

    var pct = document.createElement("span");
    pct.className = "upload-pct";
    pct.textContent = "0%";

    li.appendChild(label);
    li.appendChild(bar);
    li.appendChild(pct);
    list.appendChild(li);
    return { li: li, fill: fill, pct: pct };
  }

  // Upload a single file; resolves with `true` on success.
  function uploadOne(url, file) {
    return new Promise(function (resolve) {
      var row = makeRow(file.name);
      var fd = new FormData();
      fd.append("file", file);

      var xhr = new XMLHttpRequest();
      xhr.open("POST", url);

      xhr.upload.onprogress = function (e) {
        if (e.lengthComputable) {
          var p = Math.round((e.loaded / e.total) * 100);
          row.fill.style.width = p + "%";
          row.pct.textContent = p + "%";
        }
      };
      // bytes are in — the server is now normalizing the file with ffmpeg
      xhr.upload.onload = function () {
        row.fill.style.width = "100%";
        row.pct.textContent = "⚙";
        row.li.classList.add("upload-processing");
      };
      xhr.onload = function () {
        row.li.classList.remove("upload-processing");
        var ok = xhr.status >= 200 && xhr.status < 400;
        row.li.classList.add(ok ? "upload-done" : "upload-failed");
        row.pct.textContent = ok ? "✓" : "✕";
        resolve(ok);
      };
      xhr.onerror = function () {
        row.li.classList.remove("upload-processing");
        row.li.classList.add("upload-failed");
        row.pct.textContent = "✕";
        resolve(false);
      };
      xhr.send(fd);
    });
  }

  document.querySelectorAll(".upload-form").forEach(function (form) {
    form.addEventListener("submit", function (e) {
      e.preventDefault();
      var input = form.querySelector("input[type=file]");
      var files = Array.prototype.slice.call(input.files || []);
      if (!files.length) return;

      var btn = form.querySelector("button");
      btn.disabled = true;
      var url = form.getAttribute("action");

      // Upload sequentially so the server runs one ffmpeg job at a time.
      var failed = 0;
      var chain = Promise.resolve();
      files.forEach(function (file) {
        chain = chain.then(function () {
          return uploadOne(url, file).then(function (ok) {
            if (!ok) failed += 1;
          });
        });
      });
      chain.then(function () {
        btn.disabled = false;
        input.value = "";
        // On a clean batch, refresh to show the new files in the library.
        // On failure, leave the list visible so the errors can be read.
        if (failed === 0) window.location.reload();
      });
    });
  });
})();
