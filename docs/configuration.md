# 配置说明

## 配置放在哪里

`.env` 保存 Compose 的镜像引用、DATA_ROOT、节点元数据和应用域名。`private/repositories.yaml` 保存接收器与 Publisher 共同使用的发布规则。`private/stargate.env`、密钥、CA 和 OC alias 也属于私有运行配置，不提交 Git。

`.env.example` 和 `config/repositories.example.yaml` 是模板。示例仓库 ID 不能代替 Gitea 实际 ID；生产 endpoint 需保留私有 CA 验证。初始化脚本会生成符合配方的私有文件，后续通过接入 CLI 添加仓库。

## 节点与域名

新部署在初始化时传入两个环境变量：

```sh
NODE_NAME=beijing-1 PUBLIC_BASE=example.org python3 scripts/prepare-private.py
```

`NODE_NAME` 默认 `node-name`，接受 1–48 位小写字母、数字和连字符，首尾必须是字母或数字。`PUBLIC_BASE` 必须显式传入，使用自己控制的小写 DNS 域名，不带协议、端口、路径或末尾的点。国际化域名请先转换为 ASCII/Punycode。

脚本生成 `img-<节点名>.<基域>`、`oss-<节点名>.<基域>` 和五个 `<服务>-<节点名>.console.<基域>` 管理域名（storage、git、auth、traefik、info），再把 `NODE_NAME`、`PUBLIC_BASE` 和解析后的 `STORAGE_DOMAIN`、`GIT_DOMAIN` 写入 `.env`。凭据、内部证书、Stargate 和 Traefik 路由使用同一组生成结果。

这里的 `.env` 是初始化输出，不会在脚本启动时自动读回作为输入。重新执行初始化仍会重写配置；不要用它修改正在运行的服务。

已有部署若尚未记录 `NODE_NAME` 和 `PUBLIC_BASE`，先备份 `.env`，根据 `private/domains.json` 中的现有地址补入对应值，保留原有 `STORAGE_DOMAIN`、`GIT_DOMAIN` 和其他字段。随后只读检查：

```sh
python3 scripts/deployment_config.py \
  --env .env \
  --domains private/domains.json
docker compose config --quiet
```

检查器不输出域名或秘密，不修改文件。它核对七个运行域名及两个 Compose 域名是否与节点元数据一致，不检查 DNS、证书有效期或服务健康。

只改 `.env` 中的 `NODE_NAME` 或 `PUBLIC_BASE` 不会更新 `private/domains.json`、Stargate 回调、内部证书或 Traefik 动态路由。迁移节点域名需要分别安排 DNS、证书、Gitea ROOT_URL/SSH 地址、OC public URL、Stargate 回调和入口路由，并处理旧链接；当前没有自动迁移命令。不要通过重跑初始化替代迁移。

`NODE_NAME` 只参与域名生成，不改变 Compose 项目、服务名、固定 IP、端口和数据目录，因此不提供同机多实例隔离。

## 发布配置字段

- `schema_version`：当前为 1。
- `state_root`：容器内任务、Git 缓存和清单目录，默认 `/state`。
- `images_root`：容器内图片版本目录，默认 `/images`。
- `bucket`：对象存储桶，配方为 attachments。入口路由和权限也按该桶配置，不能只改此字段。
- `endpoint`：Publisher 使用的 S3 根地址。配方为 `https://storage.internal:9000`，CA 从 S3_CA_FILE 指定的文件读取。
- `repositories`：仓库规则列表。自动任务和人工 publish 都必须匹配其中启用的项。

每个仓库包含：

- `key`：小写字母开头，可包含小写字母、数字和连字符，最多 48 字符；用作稳定发布身份。
- `repository_id`：Gitea 正整数仓库 ID，不以名字代替。
- `name`：管理员阅读及 CLI 核对用的 owner/repository。
- `remote`：固定 SSH 地址，不采用 Webhook 请求中的地址。
- `branch`：分支名，不带 refs/heads/。
- `enabled`：是否允许发布。禁用后，待处理任务在执行时也会被拒绝，已存在公开对象不撤下。
- `serve`：是否生成图片路由，省略时为 true。修改后需要重新 render、校验并 reload Nginx。
- `source_directory`：Git 中的导出目录。标准配方和接入 CLI 固定为 public；程序随后从导出内容的 images 子目录生成图片版本。
- `mount_path`：`/` 或 `/project-b/` 这样的一级 URL 前缀，不支持嵌套前缀。
- `max_files`：导出文件数上限，默认 10000，包含附件。
- `max_bytes`：一次导出的总字节上限，默认 1073741824（1 GiB）。
- `max_file_bytes`：单文件上限，默认 104857600（100 MiB）。

Worker 在导出前要求 images_root 文件系统的可用空间至少为 `2 × max_bytes`。这是按配置上限预留，不按本次实际文件大小计算；默认配置即使只发布小图片，也要求至少约 2 GiB 空闲。

文件读取、S3 上传和下载比对会把单个对象装入内存，不是恒定内存的流式发布。降低单文件上限可以控制内存压力。

## 修改如何生效

接收器在请求时读取配置，Worker 在每轮扫描时读取配置。不过 Compose 挂载的是单个配置文件，编辑器的原子替换可能让运行容器继续看到旧 inode。手动修改后应 validate，再重建 webhook/publisher；接入 CLI 会处理这一步。

渲染器读取配置并写出 images.conf，不直接控制 Nginx。候选文件应在 nginx -t 通过后 reload，失败时保留原服务配置。日常 Git 发布只切换 current，不修改 images.conf。

## 环境变量与凭据

应用镜像通过 `OPEN_ASSETS_CONFIG` 定位配置，通过 `S3_ACCESS_KEY_FILE`、`S3_SECRET_KEY_FILE` 和 `WEBHOOK_SECRET_FILE` 读取秘密。`GIT_SSH_COMMAND` 指定 Publisher 私钥和固定 known_hosts；不要关闭 StrictHostKeyChecking。

宿主机接入 CLI 使用 private/admin_password、Publisher 公钥、Webhook Secret 和 private/oc-cli。当前实现假设 Gitea content 管理账号、本机 13000 HTTP API、asset-stack_admin 网络及 assets-publish 专用策略；它不是任意 Git 平台的通用管理接口。

全局和仓库级 `retention` 控制本地版本保留及任务归档，默认禁用实际清理，配置项和保护规则见 [历史清理](retention.md)。

`INFO_DISK_*` 与 `INFO_INODE_*` 控制 Info 的容量提示阈值，由宿主机采集器读取；默认值与统计范围见 [Info 说明](info.md#容量提示)。

OtterIO 对象容量使用独立 open-assets-objects.timer：固定 15 分钟缓存间隔、共享 100 页/110 秒预算、每页最多 1000 对象和每秒至多一次列表请求。此间隔不依赖页面刷新，也不由 --refresh-storage 绕过；安装、续采与统计口径见 [Info 文档](info.md#otterio-对象统计)。
