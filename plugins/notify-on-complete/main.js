// Notify On Complete
// 下载完成后发一条通知（M3 打到 stderr，M5 接系统通知/UI）。
adl.onComplete((c) => {
  const size = c.total >= 1024 * 1024
    ? (c.total / 1024 / 1024).toFixed(1) + " MB"
    : c.total + " 字节";
  adl.notify("下载完成", c.path + "（" + size + "）");
  return { action: "pass" };
});
