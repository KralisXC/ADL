// Auto Concurrency
// onPlan 的 payload 里没有 URL，所以先在 onResolve 记住域名，再在 onPlan 用它决策。
let currentHost = "";

adl.onResolve((req) => {
  try { currentHost = new URL(req.url).hostname; } catch (e) { currentHost = ""; }
  return { action: "pass" };
});

// 规则：域名后缀 -> 并发数（1..64）
const RULES = [
  ["github.com", 4],
  ["githubusercontent.com", 4],
  ["sourceforge.net", 4],
  ["tencent", 1],
];

adl.onPlan((plan) => {
  for (let i = 0; i < RULES.length; i++) {
    const h = RULES[i][0], c = RULES[i][1];
    if (currentHost === h || currentHost.endsWith("." + h) || currentHost.indexOf(h) >= 0) {
      // 不超过引擎给的期望上限
      const target = Math.min(c, plan.concurrency);
      if (target !== plan.concurrency) {
        return { action: "rewrite", concurrency: target };
      }
    }
  }
  return { action: "pass" };
});
