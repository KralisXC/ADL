// 示例 UI 插件：只演示插件 UI 扩展点的能力，不做任何下载操作。
// 返回的是「声明式组件树」，宿主用白名单渲染器画成真实界面；
// 插件永远拿不到 DOM、不能注入 <script>、读不到 cookie。

adl.onUiRender((req) => {
  const page = (req && req.page) || 'demo';
  const now = new Date().toLocaleTimeString();
  return {
    type: 'col',
    children: [
      { type: 'heading', value: '示例插件页面 · ' + page },
      {
        type: 'text',
        value: '这是插件返回的声明式组件树，由宿主渲染。当前时间：' + now,
      },
      { type: 'divider' },
      {
        type: 'row',
        children: [
          { type: 'text', value: '进度条演示：' },
          { type: 'progress', value: 42, total: 100 },
        ],
      },
      {
        type: 'row',
        children: [
          { type: 'badge', value: '运行中', tone: 'completed' },
          { type: 'button', label: '点我触发动作', action: 'hello', kind: 'ghost' },
        ],
      },
      { type: 'divider' },
      {
        type: 'text',
        value: '提示：动作与命令需要「信任模式」；安全模式下只能看页面与设置。',
      },
    ],
  };
});

adl.onAction((req) => {
  const action = (req && req.action) || '';
  return { message: '插件收到动作：' + action + '（示例，没有任何实际效果）' };
});

adl.onCommand((req) => {
  return { message: '你好！这是来自示例插件的命令响应。' };
});
