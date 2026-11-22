// 示例 UI 插件：只演示插件 UI 扩展点的能力，不做任何下载操作。
// 返回的是「声明式组件树」，宿主用白名单渲染器画成真实界面；
// 插件永远拿不到 DOM、不能注入 <script>、读不到 cookie。
//
// 组件词汇表见 plugins/README.md「UI 组件」一节。要点：
//   - 只用语义色名（tone: accent/warn/danger/muted），不要写 RGB ——
//     硬编码色在换主题时会变成一块突兀的补丁；
//   - 不认识的 type 宿主会画成 [xxx] 占位，不会崩，所以可以放心用新组件。

adl.onUiRender((req) => {
  const page = (req && req.page) || 'demo';
  const now = new Date().toLocaleTimeString();
  return {
    type: 'col',
    gap: 10,
    children: [
      { type: 'heading', value: '示例插件页面 · ' + page },
      {
        type: 'text',
        value: '这是插件返回的声明式组件树，由宿主渲染。当前时间：' + now,
      },
      { type: 'label', value: '状态与徽标' },
      {
        type: 'row',
        gap: 12,
        children: [
          { type: 'status', value: '运行中', tone: 'accent' },
          { type: 'status', value: '已完成', tone: 'completed' },
          { type: 'status', value: '已暂停', tone: 'warn' },
          { type: 'status', value: '失败', tone: 'danger' },
          { type: 'badge', value: '实验性', tone: 'muted' },
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '统计卡片墙（grid）' },
      {
        type: 'grid',
        columns: 3,
        gap: 10,
        children: [
          { type: 'stat', value: '1.2 GB', label: '今日下载' },
          { type: 'stat', value: '16', label: '并发连接' },
          { type: 'stat', value: '99.7%', label: '成功率' },
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '键值对（kv）' },
      {
        type: 'kv',
        labelWidth: 110,
        items: [
          { key: '插件版本', value: '0.1.0' },
          { key: '运行模式', value: '信任 / 安全（按设置）', tone: 'accent' },
          { key: '入口文件', value: 'main.js' },
          { key: '最近错误', value: '无', tone: 'muted' },
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '插件表格（table）' },
      {
        type: 'table',
        columns: ['分片', '状态', '大小'],
        rows: [
          ['#001', '已完成', '4 MiB'],
          ['#002', '已完成', '4 MiB'],
          ['#003', '下载中', '2.3 MiB'],
          ['#004', '等待', '0 B'],
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '进度与控件' },
      {
        type: 'row',
        gap: 12,
        children: [
          { type: 'progress', value: 42, total: 100 },
          { type: 'progress', value: 0.86, label: '插件加载' },
        ],
      },
      {
        type: 'row',
        gap: 8,
        children: [
          { type: 'button', label: '主要动作', action: 'hello', kind: 'primary' },
          { type: 'button', label: '次要动作', action: 'hello' },
          { type: 'button', label: '文字按钮', action: 'hello', kind: 'ghost' },
        ],
      },
      { type: 'toggle', label: '示例开关（改动会写到插件设置）', value: true, setting: 'show_tip' },
      {
        type: 'select',
        label: '示例下拉',
        value: 'A',
        options: ['A', 'B', 'C'],
        setting: 'greeting',
      },
      { type: 'divider' },
      { type: 'label', value: '提示条（alert）' },
      {
        type: 'alert',
        tone: 'accent',
        title: '这是一条普通提示',
        value: 'alert 有底色块和左侧色条，能承载一整句话，比 badge 更合适。',
      },
      {
        type: 'alert',
        tone: 'warn',
        title: '注意',
        value: 'tone 换成 warn / danger 会跟着主题换底色，不要自己写 RGB。',
      },
      { type: 'divider' },
      { type: 'label', value: '步骤条（steps）' },
      {
        type: 'steps',
        current: 1,
        items: ['解析链接', '探测文件', '分片下载', '校验写入'],
      },
      { type: 'divider' },
      { type: 'label', value: '折叠面板（collapse）' },
      {
        type: 'collapse',
        title: '展开看详细信息',
        open: false,
        children: [
          { type: 'text', value: '折叠面板里的内容在展开前不占高度。' },
          { type: 'code', value: 'collapse.open = false 时默认收起' },
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '限高滚动（scroll）与加载中（spinner）' },
      {
        type: 'scroll',
        id: 'demo-log',
        maxHeight: 120,
        children: [
          { type: 'code', value: '00:00:01 探测完成，大小 16.0 MB，支持 Range' },
          { type: 'code', value: '00:00:02 分片表建立，4 个分片' },
          { type: 'code', value: '00:00:03 分片 #001 开始下载' },
          { type: 'code', value: '00:00:04 分片 #002 开始下载' },
          { type: 'code', value: '00:00:05 分片 #003 开始下载' },
          { type: 'code', value: '00:00:06 分片 #004 开始下载' },
          { type: 'code', value: '00:00:07 速度 96.2 MB/s' },
        ],
      },
      { type: 'spinner', label: '正在等待插件异步结果…', size: 14 },
      { type: 'divider' },
      { type: 'label', value: '一键复制（copy）' },
      {
        type: 'copy',
        label: '复制哈希',
        value: 'sha256: 04fe8f6799207557…36e38719',
      },
      { type: 'divider' },
      { type: 'label', value: '带色调的表格单元格' },
      {
        type: 'table',
        columns: ['分片', '状态', '大小'],
        rows: [
          ['#001', { value: '已完成', tone: 'accent' }, '4 MiB'],
          ['#002', { value: '已完成', tone: 'accent' }, '4 MiB'],
          ['#003', { value: '下载中', tone: 'warn' }, '2.3 MiB'],
          ['#004', { value: '失败', tone: 'danger' }, '0 B'],
        ],
      },
      { type: 'divider' },
      { type: 'label', value: '等宽文本（code / timestamp）' },
      { type: 'code', value: 'sha256: 04fe8f6799207557…36e38719' },
      { type: 'timestamp', value: '渲染于 ' + now },
      {
        type: 'text',
        value: '提示：动作与命令需要「信任模式」；安全模式下只能看页面与设置。',
        dim: true,
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
