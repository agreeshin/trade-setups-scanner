/* global document */
(() => {
  let theme = "system";
  try {
    const saved = localStorage.getItem("trade-setups-theme");
    if (saved === "light" || saved === "dark" || saved === "system") theme = saved;
  } catch {
    // Storage may be denied; the system theme still works without persistence.
  }
  document.documentElement.dataset.theme = theme;
})();
