# 组件版本核查与 Stargate 升级（2026-11-08）

本次从应用基线 `b371fdc75508593581dda6bf4f31b3952ec93985` 核查运行组件，升级 Stargate，未改动发布业务代码。此前验证记录不作为本次重新执行过的测试。

## 版本选择

Stargate 从 1.3.1 升级为 [1.3.5](https://github.com/soulteary/stargate/releases/tag/v1.3.5)。镜像固定为：

```text
ghcr.io/soulteary/stargate:1.3.5@sha256:98ec7f90728b0b0a6271318d755baeb5295a581de29d36f03fa5f0f528d4107b
```

镜像索引包含 Linux amd64 与 arm64；核对索引原始字节的 SHA-256 与仓库返回摘要一致。平台子清单分别为：

- amd64：`sha256:1a18b0e9b8e11c66d687f30f1c6269bf2866742463f8e6cdb8f84d5f0d997580`。
- arm64：`sha256:a8fa89d87a955933d5fc913e138fb4028c6f912416910c2d6f7c19ea48dde8e2`。

本配方仍使用单实例密码认证，不部署独立 Warden、Herald、TOTP 或 Redis。上游依赖版本变化不等于这些服务已被安装或升级。外部认证、共享会话或 HMAC 部署需另按上游要求验证，见[升级说明](../upgrade.md#stargate-131-升级到-135)。

核查时，其他组件没有需要切换的适用正式版本：

- [OtterIO](https://github.com/soulteary/otterio/releases/tag/RELEASE.2026-10-28T07-52-43Z) 与 [OC](https://github.com/soulteary/oc/releases/tag/RELEASE.2026-10-28T07-34-44Z) 保留现有配对。
- [Gitea 28.3.0](https://github.com/go-gitea/gitea/releases/tag/v28.3.0)、[Traefik 3.7.17](https://github.com/traefik/traefik/releases/tag/v3.7.17) 和 [Certs Maker 3.8.2](https://github.com/soulteary/certs-maker/releases/tag/v3.8.2) 保持原版本与摘要。
- [Nginx 稳定分支](https://nginx.org/en/download.html)仍为 1.30.6，本次不切换至主线分支。
- Python 兼容分支仍为 3.13.18，保留已验证的基础镜像摘要。PyPI 核查未发现其余已固定依赖的新适用版本；虽然 python-dateutil 已发布 4.0.0，[botocore 1.43.126 的依赖约束](https://pypi.org/pypi/botocore/1.43.126/json)为 `python-dateutil>=2.1,<3.0.0`，因此保留 2.9.0.post0。

这些是当次核查结果，不是持续跟踪的“最新版本”清单。

## 隔离验证

使用独立 Linux aarch64 Docker-in-Docker 环境，Docker Engine 28.5.2、Compose 2.40.3；不挂载生产配置、数据或 Docker socket，不发布宿主机端口。候选源码为上述基线及新的 `.env.example`，镜像按固定摘要拉取。

- 完整运行 `tests/e2e/fresh-stack.sh`，退出码为 0。完成初始化、拒绝重复初始化、CLI 新增和重复接入、前缀冲突拒绝。
- 两个空仓库各完成首次 Push 和后续更新，四个任务均为 published。HTTPS 图片、附件、特殊字符路径和图片 Range 校验通过。
- 启动测试 Info 后，额外检查四个管理入口：匿名跳转、密码登录、固定落地路径、认证后读取、认证域名退出和错误密码拒绝均通过。
- 对当前图片和对应版本对象逐字节哈希核对，图片与对象 Range 均通过。

测试入口使用临时 CA 与 `acceptance.invalid`，不验证公共 DNS/ACME。认证退出检查仅覆盖认证主机上的会话退出，不作为所有已交换管理主机会话同时注销的证明。本次没有重新执行浏览器上传、所有外部认证模式或压力测试。

## 已有部署验收

升级前备份应用私有配置、原镜像引用及 Traefik 配置、挂载文件和运行信息。保留旧镜像，不轮换密码、会话交换密钥、CA 或公网证书。

仅更新 Stargate 镜像并重建网关，其他八个容器 ID 未改变。升级后九个容器健康，原有应用私有文件及 Traefik 配置、证书和凭据摘要保持一致。四个管理入口的认证检查、公开图片和附件哈希与 Range 检查通过；维护标记移除，原有健康恢复定时器恢复，doctor 返回 ok。

线上验收读取已有内容，没有向业务仓库 Push 或通过 OC 改写对象。备份、完整日志、真实域名和凭据均保存在私有位置，不进入公开仓库。
