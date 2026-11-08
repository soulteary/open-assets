# 环境要求

部署、使用和开发需要的工具不同。先按你的任务准备环境；组件版本与镜像摘要统一以 [`.env.example`](../.env.example) 和 [Dockerfile](../Dockerfile) 为准，升级见[组件维护](upgrade.md)。

## 部署服务器

- 一台支持 Docker Engine 和 Compose 插件的 Linux 服务器，有 sudo/root 权限和本地 POSIX 文件系统。当前配方是单机、单 Publisher，不提供同机多实例隔离。
- Linux amd64 或 arm64。Certs Maker 安装器只接受这两种架构；其他架构不是本配方的部署目标。
- 宿主 Python、PyYAML、bcrypt、Git、OpenSSH 客户端、OpenSSL、curl。新服务器指南以 Ubuntu 24.04 LTS 的系统 Python 3.12 为例；应用容器使用 Python 3.13，不需要在服务器另外安装相同版本。
- 可下载固定镜像、Python 包和 Certs Maker 发行包的网络。访问 GitHub 超时不应通过关闭 TLS 验证解决。
- 自己控制的域名与 DNS 配置权限。默认产生七个主机名，需要分别指向服务器；管理域名含两层子域名，单个 `*.example.org` 证书不能覆盖它们。
- 公网 TCP 80/443 提供入口；Git SSH 默认为 TCP 2222；运维 SSH 保留来源限制。S3/Admin/OC 端口不直接公开。
- 无冲突的 `172.29.19.0/24` 与 `172.29.20.0/24` 网络。更改时要同时检查 Compose、Gitea 出站白名单、网关地址与可信代理，见[部署说明](deployment.md)。

Docker 发布端口可能绕过仅针对宿主 INPUT 链的防火墙规则，需同时检查云安全组与 Docker 的转发规则，见 [Docker Ubuntu 安装说明](https://docs.docker.com/engine/install/ubuntu/)。不要仅凭 UFW 状态判断端口未公开。

## 容量与验证基线

项目没有承诺一个适用于所有内容规模的最低 CPU、内存或磁盘值。[历史采样](history/2026-11-07-performance.md)来自 2 核、约 3.63 GiB 内存节点，是参考样本，不是容量保证。

默认每次导出的总上限为 1 GiB，图片数据所在文件系统在导出前要求至少 `2 × max_bytes` 的空闲空间，即约 2 GiB。除此之外，还要容纳系统、镜像、Gitea 仓库、Publisher Git 缓存、历史对象、图片双副本、暂存和备份。不能把 2 GiB 理解为整机所需磁盘大小。

[完整部署复验](history/2026-11-08-fresh-install.md)记录 Linux aarch64、Docker Engine 28.5.2、Compose 2.40.3、宿主 Python 3.12.16；[网关升级](history/2026-11-08-stargate-upgrade.md)使用同类隔离环境并验证已有部署。上述版本是测试基线，不是最低版本声明，也不表示所有 Linux 发行版均已测试。新服务器的 Docker 安装命令与 ACME 公网申请仍需在目标环境验收。

## 使用者的电脑

网页管理只需要支持当前应用的浏览器、HTTPS 访问地址和管理员分配的凭据。Git 发布另需 Git、OpenSSH 客户端及个人 Gitea SSH 公钥授权。生成示例文件和编码链接的命令使用 Python 3；不要求在个人电脑安装 Docker。

## 开发与测试

Python 3.13、Git，以及支持 `node:test` 的 Node.js；Python 依赖从 `requirements.txt` 安装，Ruff 版本与 CI 一致。当前 CI 固定 Python 3.13，但没有显式固定 Node 版本，因此不声明 Node 最低兼容版本。提交问题或测试结果时记录 `node --version`；不要把测试工具的最低版本推断成网页浏览器的兼容范围。

宿主部署脚本另依赖 PyYAML、bcrypt、OpenSSL 和 OpenSSH；仅运行 Python 单元测试不需要生产凭据或云服务器。完整命令见[开发测试](development.md)，真实部署测试必须使用可丢弃的独立 Docker daemon。
