# Open Assets

Open Assets 将 Gitea 仓库中的文件发布到 OtterIO 和 Nginx。开发者通过 SSH Push 更新内容；发布程序导出指定提交的 `public/`，把附件保存到带 commit SHA 的对象路径，再切换图片服务的当前版本。

项目适用于自己维护图片、文档和下载文件的单机环境。它包含发布程序、仓库接入 CLI、Docker Compose 配置，以及接入现有 Traefik 的脚本。许可证为 [Apache-2.0](LICENSE)。

## 发布后的地址

假设仓库 key 为 `main-images`，图片前缀为 `/`，提交包含：

```text
public/
├── images/2026/11/07/logo.png
└── documents/manual.pdf
```

图片通过固定地址访问：

```text
https://img-toh1.lab.dev/2026/11/07/logo.png
```

附件通过版本地址访问：

```text
https://oss-toh1.lab.dev/attachments/repositories/main-images/releases/<完整SHA>/documents/manual.pdf
```

图片地址在下一次成功发布后指向新版本，默认缓存 300 秒。附件地址包含完整 SHA，适合在文章或版本说明中引用；项目不会自动删除历史附件。文档中的 `lab.dev` 是占位域名，不能原样用于生产配置。

## 阅读顺序

- 已经部署：从 [实际使用指南](docs/usage.md) 开始，查看登录凭据、设置个人 SSH 密钥、克隆仓库并 Push。
- 首次部署：先读 [部署说明](docs/deployment.md)，确认固定路径、网络、证书和初始化脚本的适用范围。
- 接入其他仓库：使用 [仓库接入 CLI](docs/cli.md)。命令协调 Gitea、S3 权限和 Nginx 映射，不会替你提交内容。
- 调整配置：参考 [配置说明](docs/configuration.md) 和 [仓库与 URL 配置](docs/repositories.md)。
- 处理失败、回滚或备份：参考 [运行维护](docs/operations.md) 和 [故障排查](docs/troubleshooting.md)。
- 理解程序行为和验证范围：参考 [发布流程](docs/architecture.md)、[验收记录](docs/acceptance.md) 与 [安全说明](SECURITY.md)。

## 日常操作

以下命令在已经完成初始化的服务器执行：

```sh
cd /app/open-assets
sudo ./scripts/open-assets repo ensure content/project-b \
  --key project-b \
  --mount-path /project-b/
```

返回 `status: ready` 后，开发者使用个人 Gitea SSH 密钥克隆仓库，把图片放进 `public/images/`，把其他附件放进 `public/` 下的相应目录，然后提交到配置分支。接入命令创建的是空私有仓库；Publisher 的只读 Deploy Key 不用于开发者 Push。

查看发布结果：

```sh
cd /app/open-assets
sudo docker compose exec -T publisher python -m open_assets status
```

只有目标仓库与 SHA 对应的任务显示 `published`，才表示该次发布完成。`git push` 成功和 Webhook 返回 202 都不是发布成功的标志。

## 服务组成

Gitea 向内部 Webhook 接收器发送签名事件；接收器写入文件队列；单个 Publisher 串行处理任务，上传 OtterIO 并准备 Nginx 图片目录。接收器和 Publisher 使用同一个应用镜像，以不同命令和挂载权限运行。

现有 Traefik 提供 HTTPS 和路由。公开图片、附件允许匿名读取；Gitea 网页、OC Console 和 Traefik Dashboard/API 经过 Stargate。Gitea 和 OC 保留各自的应用认证。Git SSH 使用 Gitea 密钥，不经过 HTTP 认证层。

宿主机仓库接入 CLI 使用本机 Docker 和私有管理员配置。应用容器不挂载 Docker socket，也不获得这项管理权限。

## 使用限制

当前配方固定使用节点名 `toh1`、Compose 项目名 `asset-stack`、`/app/open-assets` 和 `/data/asset-stack`。适用范围是单机 Docker Compose、一个 Publisher 和本地 POSIX 文件系统；改变路径、项目名或拓扑前需要检查脚本中的固定值。

发布程序不执行内容仓库脚本，不支持 LFS、submodule、符号链接或隐藏路径。图片按扩展名限制为 PNG/JPEG/GIF/WebP/AVIF/ICO，不检测文件格式或恶意内容。每个版本需包含 `public/images/` 中至少一张图片；其余文件作为附件发布。

失败任务不会自动重试。程序重启会重新处理遗留的 `running` / `images_prepared` 任务，但自动任务若已不是远端分支头，会被标记为 `superseded`。对象上传、图片切换和任务记录之间没有跨系统事务；部分对象可能在任务完成前已能读取。

## 本地检查

发布程序使用 Python 3.13 和 Git；宿主机初始化另需 PyYAML、bcrypt、OpenSSL、OpenSSH 及 Docker Compose。详细依赖和启动顺序见部署文档。

```sh
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
python -m unittest discover -s tests -v
```

普通测试使用临时目录和回环 HTTP 端口，不连接生产服务器。独立的 [从零部署脚本](tests/e2e/README.md) 仅用于可丢弃的空环境；执行结果见 [隔离部署记录](docs/fresh-install.md)。仓库中的 [CI](.github/workflows/test.yml) 另外运行 Ruff 和镜像构建。一次部署的成功记录不代表所有浏览器、硬件或升级路径均已验证。
