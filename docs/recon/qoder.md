# qoder 签到侦察

- 日期 / 客户端版本：
- 侦察人：

## 签到接口
- [ ] Method + URL：
- [ ] 必需 Headers（含鉴权头格式）：
- [ ] 请求 Body 与签名参数（如有，记录算法线索）：
- [ ] 成功响应样例（脱敏）：

## 状态查询接口
- [ ] 存在？Method + URL + 响应样例：

## 本机登录态
- [ ] token/session 存储路径：
- [ ] 加密方式（明文 / DPAPI / Chromium Local State AES 等）：
- [ ] 提取步骤（可复述）：

## 风控观察
- [ ] 是否有验证码/设备绑定？触发条件？

## 结论
- [ ] 可行模式：自动签到 / 仅提醒
- [ ] 未决问题：

---
抓包方法：优先 mitmproxy 或 Fiddler Classic 做系统代理；若产品为 Electron 且证书固定，
用 `--remote-debugging-port=9222` 启动后在 Chrome DevTools Network 面板观察（仅自己的账号流量）。
