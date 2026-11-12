(() => {
  let count = 0;
  let english = false;
  const button = document.getElementById('counter');
  function render() {
    document.getElementById('title').textContent = english ? 'Your capability pack is ready' : '能力包已就绪';
    document.getElementById('description').textContent = english ? 'Build your application page here.' : '在这里开发你的业务页面。';
    button.textContent = english ? `Click count: ${count}` : `点击计数：${count}`;
  }
  button.addEventListener('click', () => { count++; render(); });
  if (window.tronPage) window.tronPage.onContext(context => {
    english = context.locale.startsWith('en');
    document.getElementById('context').textContent = `${context.packId} · ${context.locale}`;
    render();
  });
  render();
})();
