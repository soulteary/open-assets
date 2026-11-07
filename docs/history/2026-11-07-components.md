# 2026-11-07 组件选择记录

以下版本属于当时的升级记录。当前基线以仓库 `.env.example` 为准。

## 2026-11-07 候选版本依据

- [Stargate 1.3.1](https://github.com/soulteary/stargate/releases/tag/v1.3.1)：包含会话撤销和浏览器绑定的跨域交换；需重新登录，不混用新旧交换处理器。
- [Gitea 28.3.0](https://github.com/go-gitea/gitea/releases/tag/v28.3.0)：升级前保全数据库、仓库、配置和 SSH 主机密钥。
- [Traefik 3.7.17](https://github.com/traefik/traefik/releases/tag/v3.7.17)：核对 [3.7 迁移说明](https://doc.traefik.io/traefik/v3.7/migrate/v3/)。现有部署仍独立管理，`.env` 中的 TRAEFIK_IMAGE 是建议版本，不会由 asset-stack 自动启动第二个入口实例。
- [Nginx 1.30.6](https://nginx.org/en/download.html)：选择稳定分支；查询时 mainline 为 1.31.8，不混称为同一发布分支。
- [Certs Maker 3.8.2](https://github.com/soulteary/certs-maker/releases/tag/v3.8.2)：使用修复自定义有效期及 Firefox SAN 的正式版本。
- [OtterIO](https://github.com/soulteary/otterio/releases/tag/RELEASE.2026-10-28T07-52-43Z) 与 [OC](https://github.com/soulteary/oc/releases/tag/RELEASE.2026-10-28T07-34-44Z)：查询时仍是现有正式配对，保留原摘要，不为“升级”而更换未验收产物。

镜像固定版本与摘要。Python 保留当前 3.13 基础镜像的已核对摘要；运行依赖升级到 requirements.txt 中的固定版本。最终服务器验收范围与结果记录在 [验收文档](../acceptance.md) 和 [性能记录](../performance.md)。


## Warden、Herald 与 Stargate 1.3.1 的适用范围

再次查询正式发布后，Stargate 基线跟进 [1.3.1](https://github.com/soulteary/stargate/releases/tag/v1.3.1)，其中包含 OTP 登录上下文绑定、TOTP 配额错误和 SDK 诊断处理；本配方的密码模式不使用 OTP/TOTP 修复路径。程序中编译的 Warden/Herald SDK 版本随该镜像更新，不代表部署了独立 provider 服务。

[Warden 1.6.1](https://github.com/soulteary/warden/releases/tag/v1.6.1) 修复 opt-in SDK 自定义日志可能暴露请求 URL、查询标识和响应体的问题；默认 NoOpLogger 不因此被认定为泄漏。修复需要重建 SDK 消费者，不能只替换独立 Warden 服务。[Herald 1.4.1](https://github.com/soulteary/herald/blob/v1.4.1/CHANGELOG.md) 修复 TOTP 确认配额的 429 被掩盖为 502，并需匹配的 herald-totp 2.0.1 服务。

当前 WARDEN_ENABLED 和 HERALD_ENABLED 均为 false，因此不新增这两个服务或 Redis。未来启用时，再按其正式迁移说明保全持久状态、幂等记录、nonce 和加密密钥，完成独立验收；当前记录不覆盖这些 provider 的上线。
