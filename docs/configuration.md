# 配置说明

首次部署会生成私有配置，日常使用通常不需要逐项手工修改。配置仓库优先使用[接入 CLI](cli.md)，调整限制和维护策略时再编辑对应文件。

本文命令在服务器执行。涉及私有文件的编辑与备份时使用管理员 shell：

```sh
sudo -i
cd /app/open-assets
```

不要把 `.env.example` 或示例 YAML 直接覆盖到已有部署；里面的占位值不能代替实际仓库 ID、域名或凭据。

## 配置放在哪里

按要修改的内容选择文件：

- `.env`：镜像引用、`DATA_ROOT`、节点元数据、应用域名，以及部分 Info 采集参数。
- `private/repositories.yaml`：仓库身份、分支、图片路径、发布预算及历史保留策略。
- `private/domains.json`：已生成的实际入口域名，供路由与链接工具读取。
- `private/stargate.env`、密钥、CA 和 OC alias：认证与服务访问配置。

这些都是私有运行材料，不提交 Git。改文件前备份；是否需要重新创建容器、更新路由或证书，取决于修改内容，见[修改如何生效](#修改如何生效)。

`.env.example` 和 `config/repositories.example.yaml` 是模板。示例仓库 ID 不能代替 Gitea 实际 ID；生产 endpoint 需保留私有 CA 验证。初始化脚本会生成符合配方的私有文件，后续通过接入 CLI 添加仓库。

## 节点与域名

下面仅用于尚未准备的新部署，不是给现有部署改名的命令。先将示例基域替换为自己控制的域名，再传入两个环境变量：

```sh
NODE_NAME=beijing-1 PUBLIC_BASE=example.org python3 scripts/prepare-private.py
```

`NODE_NAME` 默认 `node-name`，接受 1–48 位小写字母、数字和连字符，首尾必须是字母或数字。`PUBLIC_BASE` 必须显式传入，使用自己控制的小写 DNS 域名，不带协议、端口、路径或末尾的点。国际化域名请先转换为 ASCII/Punycode。

脚本生成 `img-<节点名>.<基域>`、`oss-<节点名>.<基域>` 和五个 `<服务>-<节点名>.console.<基域>` 管理域名（storage、git、auth、traefik、info），再把 `NODE_NAME`、`PUBLIC_BASE` 和解析后的 `STORAGE_DOMAIN`、`GIT_DOMAIN` 写入 `.env`。凭据、内部证书、Stargate 和 Traefik 路由使用同一组生成结果。

`.env` 是首次准备的输出，不作为准备脚本的输入。已有状态会被初始化保护拒绝；修改运行配置请编辑对应文件并校验。

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

## 常见修改：调整文件限制

例如，只将某个仓库的单文件上限从默认 100 MiB 调整为 50 MiB：先备份 `private/repositories.yaml`，找到该仓库的 key，修改它的 `max_file_bytes`，保留其余字段。

```yaml
# 这是已有仓库条目中的一个字段，不是完整配置。
max_file_bytes: 52428800
```

`max_file_bytes` 限制单个文件，`max_bytes` 限制整个 `public/`，`max_files` 限制文件数量。修改上限不会自动清理已经发布的内容。按下面的[生效步骤](#修改如何生效)校验并应用；调大限制前还要检查磁盘预留和 Git/队列预算。

## 发布配置字段

- `schema_version`：当前为 1。
- `state_root`：容器内任务、Git 缓存和清单目录，默认 `/state`。
- `images_root`：容器内图片版本目录，默认 `/images`。
- `bucket`：对象存储桶，配方为 attachments。入口路由和权限也按该桶配置，不能只改此字段。
- `endpoint`：Publisher 使用的 S3 根地址。配方为 `https://storage.internal:9000`，CA 从 S3_CA_FILE 指定的文件读取。
- `publication.reference_releases`：默认 false。启用前部署只读版本 URL 解析服务和引用保护，详见[引用式版本发布](performance.md#引用式版本发布可选)。
- `publication.object_workers`：对象上传与读回校验的最大并发，默认 4，允许 1 至 8 的整数；仍在整批验证通过后才写入，见[发布性能说明](performance.md#publisher-的批量读取与并发发布)。
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

Git blob 流式导出到暂存文件，S3 上传使用文件流，哈希和下载比对分块读取，不把整个对象装入内存。文件数、导出容量、Git 拉取和队列仍分别受预算限制；流式处理不等于整个进程具有固定内存占用，见[资源预算](resource-limits.md)。

## 修改如何生效

以仅调整已有仓库的文件限制或 `resource_limits` 为例：

1. 按[维护窗口](operations.md#维护窗口)暂停 Push、相关定时任务和并行管理操作，等待在途操作结束。保存私有配置备份，再停止发布服务，避免编辑期间读取不完整配置：

```sh
docker compose stop webhook publisher
```
2. 编辑 `private/repositories.yaml`，保留原有仓库 ID、remote、key 和路径。
3. 用新的一次性容器校验磁盘上的配置，成功后再重新创建消费者：

```sh
docker compose run --rm --no-deps publisher validate
# 仅在校验成功后执行：
docker compose up -d --no-build --force-recreate webhook publisher
```

4. 确认服务健康，恢复维护前的定时任务与 Push，再观察下一次发布。失败时保留错误和备份，先修复配置，不重跑初始化。

接收器在请求时读取配置，Worker 在每轮扫描时读取配置。不过 Compose 挂载的是单个文件，编辑器原子替换后，旧容器可能继续读旧文件；普通 restart 不保证重新挂载。一次性校验容器能读取新文件，但它不验证 S3 权限、Git 连通性或 Nginx 路由。

这组命令只适用于不改变访问映射的发布规则。仓库接入由 CLI 同步处理相关资源；手工修改域名、桶、图片前缀或 `serve` 还需分别更新路由、权限或 Nginx。不要仅运行上述命令就认定映射已生效。

渲染器读取配置并写出 images.conf，不直接控制 Nginx。候选文件应在 nginx -t 通过后 reload，失败时保留原服务配置。日常 Git 发布只切换 current，不修改 images.conf。

## 环境变量与凭据

应用镜像通过 `OPEN_ASSETS_CONFIG` 定位配置，通过 `S3_ACCESS_KEY_FILE`、`S3_SECRET_KEY_FILE` 和 `WEBHOOK_SECRET_FILE` 读取秘密。`GIT_SSH_COMMAND` 指定 Publisher 私钥和固定 known_hosts；不要关闭 StrictHostKeyChecking。

宿主机接入 CLI 使用 private/admin_password、Publisher 公钥、Webhook Secret 和 private/oc-cli。当前实现假设 Gitea content 管理账号、本机 13000 HTTP API、asset-stack_admin 网络及 assets-publish 专用策略；它不是任意 Git 平台的通用管理接口。

## 保留策略与看板采集

全局和仓库级 `retention` 控制本地版本保留及任务归档，默认禁用实际清理，配置项和保护规则见[历史清理](retention.md)。它不删除 OtterIO 对象；对象策略使用独立的 `object_retention`。

`INFO_DISK_*` 与 `INFO_INODE_*` 控制 Info 的容量提示阈值，由宿主机采集器读取；默认值与统计范围见 [Info 说明](info.md#健康磁盘和证书)。

OtterIO 对象容量使用独立 open-assets-objects.timer：固定 15 分钟缓存间隔、共享 100 页/110 秒预算、每页最多 1000 对象和每秒至多一次列表请求。此间隔不依赖页面刷新，也不由 --refresh-storage 绕过；安装、续采与统计口径见 [Info 文档](info.md#otterio-对象统计)。

object_retention 为独立对象保留策略，支持全局默认和仓库覆盖。默认 enabled=false、exclusive_writer=false，不继承本地 retention.enabled。清理预算、专用 IAM、预览与执行步骤见[对象保留](object-retention.md)。

恢复审计容量使用独立 `open-assets-audits.timer`，默认 15 分钟缓存，30 秒主机采集只读取过滤缓存及实际归档摘要。无需新增 S3 权限或容器；安装、手工强制采样、口径与上限见 [Info 审计统计](info.md#审计容量缓存与实际归档结果)。

Git 拉取和队列准入预算位于 `private/repositories.yaml` 顶层的 `resource_limits`，省略时也使用默认预算，完整参数见[发布资源预算](resource-limits.md)。

Git/队列的低频缓存由 `open-assets-resources.timer` 驱动；采集间隔与提示百分比位于 `.env`，准入预算仍位于 `private/repositories.yaml`。安装与字段含义见 [Info](info.md#git-缓存与全局队列)。

Info 诊断缓存间隔使用 `.env` 的 `INFO_DOCTOR_INTERVAL_SECONDS`（60–86400 秒，默认 300）。独立定时器执行 doctor，HTTP 不触发检查；见 [Info 部署诊断](info.md#部署诊断)。
