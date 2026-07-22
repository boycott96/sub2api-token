# Sub2API Pet

Sub2API Pet 是一个常驻桌面的 Codex 周额度悬浮客户端，使用 Tauri 2 构建。

## 功能

- 透明、无边框、始终置顶的桌面宠物窗口
- 每 30 秒读取 Sub2API 已缓存的 Codex 周额度
- 双击宠物或点击刷新按钮，主动查询最新周额度
- 低于 15% 时切换告警状态
- 支持多个 OpenAI/Codex 管理员账号切换
- 支持 Sub2API 两步验证、access token 自动续期
- refresh token 保存在系统钥匙串，密码不会落盘
- 菜单栏常驻、开机启动和窗口位置保留由系统窗口管理

## 使用

1. 安装并打开 `Sub2API Pet`。
2. 输入 Sub2API 站点地址，例如 `https://sub2api.example.com`。
3. 使用管理员邮箱和密码登录；开启两步验证时继续输入 6 位验证码。
4. 选择需要观察的 Codex 账号。

客户端会自动补全 `/api/v1`。远程地址必须使用 HTTPS，本地开发允许 `localhost`。

## 开发

```bash
npm install
npm run tauri dev
```

验证和打包：

```bash
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run tauri build
```

macOS 安装包生成在 `src-tauri/target/release/bundle/dmg/`。

