/* Theme before first paint: localStorage wins over the server's light default. */
(function () {
  try {
    var stored = window.localStorage.getItem("lgw-theme");
    if (stored === "dark" || stored === "light") {
      document.documentElement.setAttribute("data-theme", stored);
    }
  } catch (e) {
    /* a browser with storage disabled keeps the light default */
  }
})();
