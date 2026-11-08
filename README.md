# Open Assets

通过 Git 管理图片和附件：把文件提交到 Gitea，Open Assets 自动导出 `public/`，将版本附件上传到 OtterIO，再更新 Nginx 的当前图片目录。适合自己维护图片、文档和下载文件的单机部署。

```text
public/images/logo.png  → https://img-node-name.lab.dev/logo.png
public/documents/a.pdf  → https://oss-node-name.lab.dev/attachments/repositories/main-images/releases/<SHA>/documents/a.pdf
```

图片地址跟随当前版本，默认缓存 300 秒；附件地址包含 commit SHA。默认保留历史附件，启用对象清理并实际删除后，旧链接会失效。示例域名和节点名需替换为自己的值。

## 开始使用

- 首次安装：[部署说明](docs/deployment.md)。复用现有 Traefik，不启动第二套入口。
- 已经部署：[使用指南](docs/usage.md)。包含登录、个人 SSH 密钥、空仓库首次提交与后续更新。
- 接入新仓库：[仓库 CLI](docs/cli.md)。自动配置只读 Deploy Key、Webhook、S3 权限和图片映射，支持重复执行。
- 日常维护：[命令速查](docs/operator.md)、[故障排查](docs/troubleshooting.md)。
- 运行看板：[Info](docs/info.md)，展示健康、容量、恢复与缓存诊断；页面刷新不触发 doctor 检查。
- 全部文档：[文档目录](docs/README.md)。

在部署服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets repo ensure content/project-b --key project-b --mount-path /project-b/
sudo ./scripts/open-assets status --key project-b
```

接入返回 `ready` 后，用个人 Git SSH 密钥提交内容。只有该 SHA 的任务显示 `published` 才表示发布完成；Push 成功和 Webhook 返回 202 只表示前面的步骤已完成。

## 服务组成

![Open Assets 架构图](.github/images/open-assets-architecture.gif)

Gitea 通过内部签名 Webhook 提交任务，Publisher 串行导出和校验内容。Traefik 为 Nginx 图片和 OtterIO 附件提供 HTTPS；Gitea、OC、Traefik 管理页与 Info 由单实例 Stargate 保护，应用自身权限继续生效。应用容器不挂载 Docker socket。

Info 展示组件健康、CPU/内存、磁盘、证书、Git/队列预算和恢复记录。容量统计使用定时缓存；历史记录与当前健康分别查看。日志轮转和受限健康恢复由宿主机定时任务管理。

架构图由 [fireworks-tech-graph](https://github.com/yizhiyanhua-ai/fireworks-tech-graph/) 的 Claude 风格生成，SVG 定制了流向动画。动画表示调用方向，不表示延迟或事务边界。

## 使用前确认

- 面向单机 Docker Compose、单个 Publisher 和本地 POSIX 文件系统。配方固定 `/app/open-assets`、`/data/asset-stack` 及部分网络、端口；更改拓扑前需检查配置。
- 每个发布版本需有 `public/images/` 中至少一张图片。支持 PNG/JPEG/GIF/WebP/AVIF/ICO，不支持 SVG、LFS、submodule、符号链接和隐藏路径；不会执行仓库脚本。
- 发布使用 Git/队列预算与流式校验。预算不替代磁盘配额、容器内存限制和备份。多个系统没有联合事务，失败时可能留下部分对象。
- 初始化只用于新部署。程序拒绝已有配置或已启动的初始化；升级、改域名及新增仓库使用对应维护步骤。

更多说明：[资源预算](docs/resource-limits.md)、[恢复内容](docs/object-restore.md)、[保留策略](docs/object-retention.md)、[安全](SECURITY.md)与[风险控制](docs/risks.md)。

项目采用 [Apache-2.0](LICENSE)。参与开发见[开发测试](docs/development.md)，已验证范围见[验证记录](docs/acceptance.md)。
