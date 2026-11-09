# 组件升级与内部证书

升级先在隔离环境验证，再安排线上维护。不要通过重新执行 `open-assets prepare-private` 或 `open-assets initialize-services` 升级已有环境，它们仅用于新环境，已有或中断状态会被保护机制拒绝。

## 确认版本

仓库的 `.env.example` 记录组件版本和镜像摘要，实际运行配置位于私有 `.env`。升级时比较两份配置与发行说明，按差异安排数据库、会话和证书兼容检查。历史选择依据见[组件记录](history/2026-11-07-components.md)。

## 升级前保全

记录实际镜像 ID、摘要、启动参数、挂载和网络。保全应用 private、.env、Gitea 数据库/仓库/主机密钥、OtterIO 对象/IAM、发布记录和图片，以及原 Traefik 配置、DNS 凭据、ACME 账户与证书。停写后备份并验证归档可读；数据库格式发生变化时，不能只切回旧镜像，必须使用兼容的快照。

预拉取镜像、构建候选应用并验证内部证书，再进入维护窗口。维护期间暂停 Push，保留原镜像和完整恢复材料。不要清空 ACME，也不要启动两个写同一 ACME 文件的 Traefik。

GHCR 和 Docker Hub 是否提供某个标签取决于项目实际发布，不应机械替换镜像地址。优先使用可访问的官方镜像并核对摘要。若镜像源不可达，可在受信任环境预拉取后传输，或使用项目支持的官方发行包并校验摘要，不能改用来源不明的代理包。

## Stargate 1.3.1 升级到 1.3.5

本次模板将 Stargate 固定为 `1.3.5` 及其镜像索引摘要，其他运行组件保持原有版本。版本选择、隔离验证和部署范围见[升级记录](history/2026-11-08-stargate-upgrade.md)。正式发布说明见 [Stargate v1.3.5](https://github.com/soulteary/stargate/releases/tag/v1.3.5)；从 1.3.1 升级还需阅读 [1.3.2](https://github.com/soulteary/stargate/releases/tag/v1.3.2)、[1.3.3](https://github.com/soulteary/stargate/releases/tag/v1.3.3) 和 [1.3.4](https://github.com/soulteary/stargate/releases/tag/v1.3.4) 的变更。

本配方只使用密码认证，未启用独立 Warden、Herald、TOTP 或 Redis。上游 SDK 的更新不代表需要在这里部署这些服务；已自行启用外部认证的部署，必须遵循上游的依赖升级顺序、HMAC key ID 和共享会话状态要求，不能直接套用密码模式的验收结果。

已有部署不会因为更新 `.env.example` 自动升级。在[维护窗口](operations.md#维护窗口)内：

1. 记录旧镜像和运行状态，备份私有 `.env`、`private/stargate.env`、凭据和证书。保留原密码、会话交换密钥、回调允许列表和可信代理。
2. 预拉取 `.env.example` 中固定的新镜像，仅更新私有 `.env` 的 `STARGATE_IMAGE`；不要整份覆盖 `.env`，也不要重新初始化。
3. 在 `/app/open-assets` 的管理员 shell 中校验并重新创建网关：

```sh
docker compose config --quiet
docker compose up -d --no-build --no-deps --force-recreate stargate
docker compose ps stargate
```

4. 检查所有管理入口的匿名跳转、密码登录和认证后访问，确认错误密码不能进入；分别验证 Stargate 与 OC 的退出范围。重建会清空本配方的进程内会话，需要重新登录。检查公开图片、附件和 Range 下载，确认其他服务未受影响，再恢复维护前的定时任务。

失败时保留私有日志，恢复原 `STARGATE_IMAGE` 并重新创建 Stargate。这里没有改变存储或 IAM 数据，回退网关也不会撤销其他系统写入。不得通过关闭认证、来源检查或证书验证作为回退方式。

## 使用 Certs Maker 生成十年内部证书

新初始化自动安装与模板中版本及平台 SHA-256 匹配的 Certs Maker。支持 Linux amd64/arm64，原生程序需要 OpenSSL 和 sh，不依赖 Go。发行归档仅提取预期的普通二进制文件；安装文件和校验记录保存在 private/tools，修改过的二进制会拒绝使用。

程序使用 Firefox / local CA 模式，分别为 `storage.internal` 和 OC 实际管理域名生成独立私钥、叶证书和 CA。叶证书与 CA 都要求 **3650 天**，不是公共 CA 的十年浏览器证书。对外 HTTPS 继续由 Traefik 的 ACME 证书提供。

已有部署先在私有 `.env` 补齐 `.env.example` 中的 CERTS_MAKER_VERSION 和两个平台归档摘要，然后生成一个新的候选目录：

```sh
cd /app/open-assets
bin/open-assets internal-certs \
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

按 [使用指南](usage.md) 验证真实 Git Push、签名 Hook、发布完成、图片与版本附件的字节和 Range；按 [Info](info.md) 检查所有管理入口及采集新鲜度。Stargate 升级或重启可能使旧会话和票据失效，应准备重新登录，并验证新登录、回调及退出流程。原有入口、DNS 凭据、ACME 账户和已有证书均应保留。

发生失败时暂停写入，先判断是入口、证书信任、应用配置还是数据库变化，再选择修复或恢复。回退应同时匹配代码、镜像、数据、证书与信任配置。原生发行包缓存、升级日志、快照、密钥和真实域名均留在私有位置，不提交 Git。

## 认证组件

当前配方使用 Stargate 密码模式，不部署独立 Warden 或 Herald。需要 OTP/TOTP 等认证方式时，另行配置相应 provider、权限和回调，不直接沿用密码模式的验收结论。
