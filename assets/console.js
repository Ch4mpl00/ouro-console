(() => {
  const root = document.documentElement;
  let theme;
  try { theme = localStorage.getItem('ouro-theme'); } catch (_) { /* storage can be disabled */ }
  root.dataset.bsTheme = theme || (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
  document.addEventListener('click', (event) => {
    if (event.target.closest('#theme-toggle')) {
      root.dataset.bsTheme = root.dataset.bsTheme === 'dark' ? 'light' : 'dark';
      try { localStorage.setItem('ouro-theme', root.dataset.bsTheme); } catch (_) { /* optional preference */ }
    }
  });
})();
