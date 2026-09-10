// Playwright 浏览器验收函数：先生成 write_web_statistics_fixture 快照，
// 将 assets/statistics.*、icon.png 和快照 api/statistics 放到隔离目录并通过 127.0.0.1:18237 提供。
// 通过浏览器工具 run_code 的 filename 加载；仅操作测试页，返回已通过的检查项。
async (page) => {
  await page.goto('http://127.0.0.1:18237/');
  await page.setViewportSize({ width: 1440, height: 1000 });
  const checks = [];
  // 断言失败立即停止；不会把后续未执行检查报告为通过。
  const assert = (ok, name) => { if (!ok) throw new Error(name); checks.push(name); };
  await page.waitForFunction(() => document.querySelector('#row-count').textContent === '29');
  await page.locator('#auto-refresh').uncheck();
  assert(await page.locator('#rows tr').count() === 25, 'first page 25 rows');
  assert((await page.locator('#kpis').innerText()).includes('9.5'), 'average score 9.5');
  const models = await page.locator('#model-filter option').allTextContents();
  assert(models.filter(value => value.toLowerCase() === 'glm-5.3').length === 1, 'case-normalized models');
  await page.locator('[data-sort="score"]').click();
  assert((await page.locator('#rows tr').first().locator('td').nth(7).innerText()).startsWith('10'), 'score descending');
  await page.locator('#review-filter').selectOption('accepted');
  assert(await page.locator('#row-count').textContent() === '2', 'accepted filter');
  await page.locator('#search').fill('审计与返工');
  await page.locator('#rows tr').first().click();
  const detail = await page.locator('#detail-body').innerText();
  assert(detail.includes('首评 5 / 当前 9') && detail.includes('第 1 轮 → 第 2 轮') && detail.includes('review-before'), 'review and rework evidence');
  assert(detail.includes('2.2s') && detail.includes('近第 5 条'), 'latest five reply samples');
  await page.keyboard.press('Escape');
  assert(!await page.locator('#detail').isVisible(), 'Escape closes detail');
  await page.locator('#reset').click();
  await page.locator('#next').click();
  assert(await page.locator('#rows tr').count() === 4, 'second page remainder');
  await page.locator('#previous').click();
  await page.locator('#include-deleted').check();
  assert(await page.locator('#row-count').textContent() === '30', 'include deleted');
  await page.locator('#reset').click();
  await page.locator('#search').fill('img src');
  assert(await page.locator('#row-count').textContent() === '1' && await page.locator('#rows img').count() === 0, 'untrusted title stays text');
  await page.locator('#reset').click();
  await page.locator('#review-filter').selectOption('rework');
  assert((await page.locator('#rows').innerText()).includes('没有符合筛选条件') && await page.locator('#rows td').getAttribute('colspan') === '9', 'empty state');
  await page.locator('#reset').click();
  await page.locator('#date-from').fill('2026-09-10');
  await page.locator('#date-to').fill('2026-09-09');
  assert(await page.locator('#filter-error').isVisible(), 'invalid date range');
  await page.locator('#reset').click();
  await page.route('**/api/statistics', route => route.fulfill({ status: 503, body: 'unavailable' }));
  await page.locator('#refresh').click();
  await page.waitForFunction(() => document.querySelector('#connection').textContent === '连接中断');
  assert(await page.locator('#row-count').textContent() === '29', 'last data retained on failure');
  await page.unroute('**/api/statistics');
  await page.locator('#refresh').click();
  await page.waitForFunction(() => document.querySelector('#connection').textContent === '已暂停自动刷新');
  assert(!await page.locator('#error').isVisible(), 'connection recovery');
  await page.locator('#rubric').click();
  assert((await page.locator('#detail-body').innerText()).includes('正确性'), 'rubric visible');
  await page.keyboard.press('Escape');
  assert(!(await page.locator('body').innerText()).includes('NaN'), 'no NaN metrics');
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'desktop no body overflow');
  await page.setViewportSize({ width: 390, height: 844 });
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'mobile no body overflow');
  return checks;
}
