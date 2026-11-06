# Open Assets

通过 Gitea Push 发布不可变 S3 附件及固定 URL 图片。支持多仓库白名单、签名事件、持久化队列、内容校验和图片版本回滚。Apache-2.0。

## 组件

Gitea → 内部签名 Webhook → Publisher → OtterIO 与 Nginx。现有 Traefik 提供 HTTPS，Stargate 保护 Gitea、OC Console 及 Dashboard/API。公开图片和附件不经过认证。

本项目使用 Python 3.13、Git CLI 和 boto3。Webhook 与 Worker 使用同一个镜像、不同命令和权限。内置接收器保证文件和父目录刷盘后才返回 202；第一版没有依赖外部 webhook 命令执行框架。

## 日常使用

查看密码、配置个人 SSH 密钥、Git Push 发布、生成图片及附件链接，以及新增白名单仓库，见 [实际使用指南](docs/usage.md)。

## 部署

阅读 [部署](docs/deployment.md)、[仓库配置](docs/repositories.md)、[运行维护](docs/operations.md) 和 [安全边界](SECURITY.md)。先复制 `.env.example` 并在私有目录填写配置。`private/`、`.env` 和数据目录禁止提交。

```sh
cp .env.example .env
mkdir -p private/generated
# 按部署文档准备 private/ 下凭据、TLS 和 repositories.yaml
docker compose config --quiet
docker compose build
# 新仓库首次启动前生成 Nginx 配置
docker compose run --rm --no-deps -v ./private/generated:/generated publisher render /generated
docker compose up -d
```

现有 Traefik 通过外部网络连接 `open-assets-edge` 与 `open-assets-auth`。公网只开放 80/443、受限运维 SSH 和 Gitea SSH。不要公开 OtterIO/Admin、OC、Webhook 或 Stargate 后端端口。

## 验证

```sh
python -m pip install -r requirements.txt
python -m unittest discover -s tests -v
```

发布仅导出配置的 `public/`，不执行仓库脚本；`public/images/` 支持 PNG/JPEG/GIF/WebP/AVIF/ICO。附件保留 `repositories/<key>/releases/<sha>/`，图片独立切换每仓库 current。禁止 LFS、submodule、隐藏路径和符号链接。第一版单 Worker 串行执行，不自动清理历史附件。

发布不是跨 S3 与文件系统事务。对象完整校验后才切换图片；中断可恢复。公开版本对象在逐个上传时可能提前可见，成功状态不代表原子批量可见性。
