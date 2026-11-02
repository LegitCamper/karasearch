(() => {
  const themes = ['light', 'dark', 'latte', 'frappe', 'macchiato', 'mocha', 'dracula'];
  let theme;
  try { theme = localStorage.getItem('karasearch:theme'); } catch {}
  document.documentElement.dataset.theme = themes.includes(theme)
    ? theme
    : window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
})();
