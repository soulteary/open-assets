# Open Assets

图片与文件的自托管服务，让你在自己的服务器上管理和分享内容。

为网站、应用和项目提供图片与文件访问链接。通过网页上传、下载和管理存储对象，或通过 Git 自动发布图片与附件。管理入口统一认证，服务状态、证书和存储空间可以在看板中查看。

## 项目特点

- **网页管理与 Git 自动发布**：通过浏览器管理文件；需要版本记录和自动更新时，将内容提交到 Git，由程序完成导出、校验和发布。
- **两种链接，满足不同用途**：图片使用固定地址，随当前版本更新；附件地址包含提交版本，方便引用和追溯。历史附件在对应对象保留期间有效。
- **多个仓库，独立图片路径**：为不同项目配置仓库和访问前缀，统一命令协调只读 Deploy Key、Webhook、存储权限和图片映射，支持重复执行。
- **发布可查，历史内容可恢复**：查看任务状态和校验结果；从 Git 重建已清理的历史内容，完整校验后可显式回滚图片版本。
- **日常维护集中查看**：Info 展示组件健康、磁盘、证书与历史版本占用；doctor 提供诊断和下一步建议，保留策略帮助管理存储空间。

## 服务组成

![Open Assets 架构图](.github/images/open-assets-architecture.gif)

各组件分别负责接收内容、发布文件、提供访问和日常管理：

- **Gitea**：托管内容仓库，接收 Git Push，并通知发布服务。
- **Webhook 与 Publisher**：验证来自 Gitea 的通知并排队处理，依次导出、校验和发布仓库中的文件。
- **OtterIO**：提供兼容 S3 的对象存储，保存版本附件和网页上传的文件。
- **OC**：存储管理网页，用于登录后上传、下载和管理 OtterIO 中的文件。
- **Nginx**：提供当前版本的图片访问，让图片保持固定链接。
- **Traefik**：统一提供 HTTPS 入口，将请求转发到对应服务，并提供入口管理页面。
- **Stargate**：为 Gitea、OC、Traefik 管理页和 Info 提供统一的登录保护；各应用自身的账号和权限仍然生效。
- **Info**：运行看板，展示组件健康、证书有效期、磁盘占用和发布维护状态。

公开图片和已配置公开权限的附件无需登录即可访问。应用容器不挂载 Docker socket；Info 的容量与诊断数据由定时任务采集，页面刷新不会触发存储遍历或 doctor 检查。日志轮转和受限健康恢复由宿主机定时任务管理。

## 选择适合的上传方式

**网页上传**适合直接管理存储对象。在 OC 存储管理后台上传、下载和管理文件，访问权限由存储策略决定。上传不会自动同步到 Git，也不会更新 Nginx 的图片目录；对外分享需先配置[网页公开目录](docs/web-uploads.md)，默认不自动公开所有上传内容。

**Git 发布**适合需要修改记录、版本链接和图片自动更新的内容。将文件放入仓库的 `public/` 并 Push 后，程序校验内容、上传版本附件，再更新当前图片目录。通过 SSH Push 时不需要先登录网页。

Git 发布后的链接示例：

```text
public/images/logo.png  → https://img-node-name.lab.dev/logo.png
public/documents/a.pdf  → https://oss-node-name.lab.dev/attachments/repositories/main-images/releases/<SHA>/documents/a.pdf
```

图片默认缓存 300 秒，更新后客户端可能暂时看到旧内容。历史附件默认保留；启用对象清理并实际删除后，旧链接会失效。示例域名和节点名需替换为自己的值。

Git 管理的版本目录应交由发布程序维护，避免在网页后台直接修改，造成对象与校验清单不一致。登录和访问链接的具体用法见[使用指南](docs/usage.md)。

## 开始使用

- 首次安装：先看[环境要求](docs/requirements.md)，再选择[全新服务器](docs/new-server.md)或[接入已有 Traefik](docs/deployment.md)。
- 第一次分享：[首次分享文件](docs/first-share.md)。包含 Git 发布和网页上传两条路线，以及匿名下载校验。
- 已经部署：[使用指南](docs/usage.md)。包含登录、个人 SSH 密钥、空仓库首次提交与后续更新。
- 接入新仓库：[仓库 CLI](docs/cli.md)。自动配置只读 Deploy Key、Webhook、S3 权限和图片映射，支持重复执行。
- 日常维护：[命令速查](docs/operator.md)、[故障排查](docs/troubleshooting.md)。
- 运行看板：[Info](docs/info.md)，展示健康、容量、恢复与缓存诊断；页面刷新不触发 doctor 检查。
- 全部文档：[文档目录](docs/README.md)。

在部署服务器先检查现状（以下命令不修改服务）：

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets status --key main-images
```

新增仓库请按[仓库 CLI](docs/cli.md)执行 `repo ensure`；该操作会创建或更新部署资源。接入返回 `ready` 后，用个人 Git SSH 密钥提交内容。只有该 SHA 的任务显示 `published` 才表示发布完成；Push 成功和 Webhook 返回 202 只表示前面的步骤已完成。

## 使用前确认

- 面向单机 Docker Compose、单个 Publisher 和本地 POSIX 文件系统。配方固定 `/app/open-assets`、`/data/asset-stack` 及部分网络、端口；更改拓扑前需检查配置。
- Git 发布的每个版本需有 `public/images/` 中至少一张图片。图片格式支持 PNG/JPEG/GIF/WebP/AVIF/ICO；Git 发布不支持 SVG、LFS、submodule、符号链接和隐藏路径，也不会执行仓库脚本。
- 发布使用 Git/队列预算与流式校验。预算不替代磁盘配额、容器内存限制和备份。多个系统没有联合事务，失败时可能留下部分对象。
- 初始化只用于新部署。程序拒绝已有配置或已启动的初始化；升级、改域名及新增仓库使用对应维护步骤。

更多说明：[资源预算](docs/resource-limits.md)、[恢复内容](docs/object-restore.md)、[保留策略](docs/object-retention.md)、[安全](SECURITY.md)与[风险控制](docs/risks.md)。

项目采用 [Apache-2.0](LICENSE)。参与开发见[贡献指南](CONTRIBUTING.md)和[开发测试](docs/development.md)，已验证范围见[验证记录](docs/acceptance.md)。
