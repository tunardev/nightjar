(function () {
  "use strict";

  if (typeof window.fetch !== "function") {
    return;
  }

  var params = new URLSearchParams(window.location.search);
  var job = params.get("job") || "";
  var idle = 0;
  var lastLatestRun = null;
  var timer = null;

  function statusUrl() {
    var query = "idle=" + idle;
    if (job) {
      query += "&job=" + encodeURIComponent(job);
    }
    return "/status.json?" + query;
  }

  function schedule(ms) {
    if (timer !== null) {
      window.clearTimeout(timer);
    }
    timer = window.setTimeout(poll, ms);
  }

  function poll() {
    fetch(statusUrl(), { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (data) {
        if (!data) {
          schedule(30000);
          return;
        }
        idle = data.idle_streak;
        if (lastLatestRun !== null && data.latest_run !== lastLatestRun) {
          window.location.reload();
          return;
        }
        lastLatestRun = data.latest_run;
        schedule(data.next_poll_ms);
      })
      .catch(function () {
        schedule(30000);
      });
  }

  poll();
})();
