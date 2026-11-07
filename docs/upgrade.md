# 组件升级与内部证书

升级先在实际部署验收，再更新公开版本基线。不要通过重新执行 prepare-private.py 或 initialize-services.py 升级已有环境，它们会重写配置和身份状态。

## 2026-11-07 候选版本依据

- [Stargate 1.3.1](https://github.com/soulteary/stargate/releases/tag/v1.3.1)：包含会话撤销和浏览器绑定的跨域交换；需重新登录，不混用新旧交换处理器。
- [Gitea 28.3.0](https://github.com/go-gitea/gitea/releases/tag/v28.3.0)：升级前保全数据库、仓库、配置和 SSH 主机密钥。
- [Traefik 3.7.17](https://github.com/traefik/traefik/releases/tag/v3.7.17)：核对 [3.7 迁移说明](https://doc.traefik.io/traefik/v3.7/migrate/v3/)。现有部署仍独立管理，`.env` 中的 TRAEFIK_IMAGE 是建议版本，不会由 asset-stack 自动启动第二个入口实例。
- [Nginx 1.30.6](https://nginx.org/en/download.html)：选择稳定分支；查询时 mainline 为 1.31.8，不混称为同一发布分支。
- [Certs Maker 3.8.2](https://github.com/soulteary/certs-maker/releases/tag/v3.8.2)：使用修复自定义有效期及 Firefox SAN 的正式版本。
- [OtterIO](https://github.com/soulteary/otterio/releases/tag/RELEASE.2026-10-28T07-52-43Z) 与 [OC](https://github.com/soulteary/oc/releases/tag/RELEASE.2026-10-28T07-34-44Z)：查询时仍是现有正式配对，保留原摘要，不为“升级”而更换未验收产物。

镜像固定版本与摘要。Python 保留当前 3.13 基础镜像的已核对摘要；运行依赖升级到 requirements.txt 中的固定版本。最终服务器验收范围与结果记录在 [验收文档](acceptance.md) 和 [性能记录](performance.md)。

## 升级前保全

记录实际镜像 ID、摘要、启动参数、挂载和网络。保全应用 private、.env、Gitea 数据库/仓库/主机密钥、OtterIO 对象/IAM、发布记录和图片，以及原 Traefik 配置、DNS 凭据、ACME 账户与证书。停写后备份并验证归档可读；数据库格式发生变化时，不能只切回旧镜像，必须使用兼容的快照。

预拉取镜像、构建候选应用并验证内部证书，再进入维护窗口。维护期间暂停 Push，保留原镜像和完整恢复材料。不要清空 ACME，也不要启动两个写同一 ACME 文件的 Traefik。

GHCR 和 Docker Hub 是否提供某个标签取决于项目实际发布，不应机械替换镜像地址。优先使用可访问的官方镜像并核对摘要。若镜像源不可达，可在受信任环境预拉取后传输，或使用项目支持的官方发行包并校验摘要，不能改用来源不明的代理包。

## 使用 Certs Maker 生成十年内部证书

新初始化自动安装与模板中版本及平台 SHA-256 匹配的 Certs Maker。支持 Linux amd64/arm64，原生程序需要 OpenSSL 和 sh，不依赖 Go。发行归档仅提取预期的普通二进制文件；安装文件和校验记录保存在 private/tools，修改过的二进制会拒绝使用。

程序使用 Firefox / local CA 模式，分别为 `storage.internal` 和 OC 实际管理域名生成独立私钥、叶证书和 CA。叶证书与 CA 都要求 **3650 天**，不是公共 CA 的十年浏览器证书。对外 HTTPS 继续由 Traefik 的 ACME 证书提供。

已有部署先在私有 `.env` 补齐 `.env.example` 中的 CERTS_MAKER_VERSION 和两个平台归档摘要，然后生成一个新的候选目录：

```sh
cd /app/open-assets
python3 scripts/internal_certs.py \
  --root /app/open-assets \
  --domains private/domains.json \
  --output private/tls-next
```

目标必须不存在或为空。命令不替换在线证书，生成后会验证证书链、SAN 主机名、私钥配对、CA 与叶证书的 3650 天有效期。若服务器不能下载 GitHub 发行包，可在受信任机器下载对应平台归档后传输，使用 `--archive /path/to/certs-maker.tar.gz`；仍按 `.env` 中固定的官方 SHA-256 校验。

私有布局：

```text
private/tls-next/
├── ca.crt                       # 两个公开 CA 的信任包
├── issuers/storage/root.crt
├── issuers/storage/root.key      # 仅宿主机管理员保存
├── issuers/console/root.crt
├── issuers/console/root.key
├── storage/public.crt
├── storage/private.key
├── storage/ca.crt
├── console/console.crt
├── console/console.key
└── console/ca.crt
```

issuers 中还保留生成原始文件，均按私钥材料保存。CA 私钥不挂载进业务容器。存储目录属 UID 20002，OC 目录属 UID 20004；Publisher 的 CA 副本属 UID 20010。

替换时安排维护窗口，保留旧 TLS 树；把新树安装到 private/tls，并更新 private/publisher/ca.crt、private/oc-cli/certs/CAs/ca.crt、OC/存储信任和 Traefik 动态文件中嵌入的 CA。随后重建相应容器，核对实际 TLS 链与公开读写。单文件 bind mount 更换 inode 后必须重新创建消费者；只替换磁盘文件不保证所有运行进程已加载。

十年有效期降低到期操作频率，也扩大密钥泄漏的影响时间。仍需限制权限、加密备份、监测到期，并在泄漏或身份变更时轮换；不应等到十年后才处理。更换 CA 会改变客户端信任，不能把新叶证书与旧 CA 混用。

## 验收与回退

按 [使用指南](usage.md) 验证真实 Git Push、签名 Hook、发布完成、图片与版本附件的字节和 Range；按 [Info](info.md) 检查所有管理入口及采集新鲜度。Stargate 1.3.x 升级后旧会话和票据失效，需要重新登录。原有入口、DNS 凭据、ACME 账户和已有证书均应保留。

发生失败时暂停写入，先判断是入口、证书信任、应用配置还是数据库变化，再选择修复或恢复。回退应同时匹配代码、镜像、数据、证书与信任配置。原生发行包缓存、升级日志、快照、密钥和真实域名均留在私有位置，不提交 Git。

## Warden、Herald 与 Stargate 1.3.1 的适用范围

再次查询正式发布后，Stargate 基线跟进 [1.3.1](https://github.com/soulteary/stargate/releases/tag/v1.3.1)，其中包含 OTP 登录上下文绑定、TOTP 配额错误和 SDK 诊断处理；本配方的密码模式不使用 OTP/TOTP 修复路径。程序中编译的 Warden/Herald SDK 版本随该镜像更新，不代表部署了独立 provider 服务。

[Warden 1.6.1](https://github.com/soulteary/warden/releases/tag/v1.6.1) 修复 opt-in SDK 自定义日志可能暴露请求 URL、查询标识和响应体的问题；默认 NoOpLogger 不因此被认定为泄漏。修复需要重建 SDK 消费者，不能只替换独立 Warden 服务。[Herald 1.4.1](https://github.com/soulteary/herald/blob/v1.4.1/CHANGELOG.md) 修复 TOTP 确认配额的 429 被掩盖为 502，并需匹配的 herald-totp 2.0.1 服务。

当前 WARDEN_ENABLED 和 HERALD_ENABLED 均为 false，因此不新增这两个服务或 Redis。未来启用时，再按其正式迁移说明保全持久状态、幂等记录、nonce 和加密密钥，完成独立验收；当前记录不覆盖这些 provider 的上线。
