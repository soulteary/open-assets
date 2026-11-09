# 部署说明

这份指南用于在一台 Linux 服务器上首次部署 Open Assets，复用一套已经工作的 Traefik。没有现有入口时，选择[全新服务器路线](new-server.md)，其中包含 Docker 安装和独立 Traefik；不要同时执行两条路线的入口部署。完成后，你可以登录网页管理文件，也可以通过 Git 发布图片与附件。

部署会依次准备私有配置、初始化存储和仓库、接入 HTTPS，最后完成一次真实发布。以下步骤需要服务器管理员权限；还需要可用的域名、DNS 配置权限，以及访问现有 Traefik 配置的权限。完整工具、容量和验证基线统一见[环境要求](requirements.md)。

源码、私有配置和数据分别放在 `/app/open-assets`、`/app/open-assets/private` 和 `/data/asset-stack`。节点名由 `NODE_NAME` 指定，默认 `node-name`；Compose 项目名固定为 asset-stack。

准备迁移现有服务时，先备份原 Compose、数据、DNS 凭据和 ACME 状态，并确认恢复方法。下面的初始化步骤只用于空的新部署，不用于覆盖已有运行目录。

## 1. 检查环境

先安装并启动 Docker Engine 和 Docker Compose 插件，再准备 Git、curl、Python 3、PyYAML、bcrypt、OpenSSL 和 OpenSSH 客户端。下面的 apt 命令只安装宿主机脚本依赖，不安装 Docker。应用容器使用的 Python 依赖由 Dockerfile 安装，宿主机脚本另外使用系统 Python。

Ubuntu 可安装宿主机依赖：

```sh
sudo apt update
sudo apt install git curl python3 python3-yaml python3-bcrypt openssl openssh-client
sudo docker version
sudo docker compose version
python3 -c 'import yaml, bcrypt'
```

上面的 Docker 命令应正常返回版本，Python 导入应无报错。服务器需要能下载模板固定的容器镜像、Python 依赖和 Certs Maker 发行包；无法下载时先排查网络，镜像来源选择见[升级说明](upgrade.md)。

使用数据盘时先完成 /data 挂载；只有系统盘时，先创建专用 /data 目录。初始化脚本创建子目录，但不挂载磁盘。随后查看磁盘、内存和现有网络，确认 172.29.19.0/24 与 172.29.20.0/24 没有冲突：

```sh
# 仅使用系统盘、且 /data 不是待挂载数据盘时，先创建目录：
# sudo install -d -m 0755 /data
df -h /data
free -h
ip route
sudo docker network ls
```

172.29.19.0/24 用于 Traefik 与 Stargate，172.29.20.0/24 用于独立内部 Webhook 网络；两者地址在 Compose、路由生成脚本和 Stargate 可信代理配置中相互对应。若有冲突，需一起修改并重新验证，不能只改一处。

Gitea 28.3 的 Webhook 出站策略要求显式允许私网 IP。配方通过 WEBHOOK_SUBNET / WEBHOOK_IP 为接收器分配独立固定地址，仅允许 webhook 和这一 IP。若有冲突，在私有 .env 中一起调整网段与地址；不要放开全部私网。

默认发布规则会在图片数据所在文件系统预留约 2 GiB 空闲空间，即使首次只上传小图片也需要满足；实际还要为容器镜像、Git 仓库和历史内容留空间，见[资源预算](resource-limits.md)。

只有 Traefik 接收公网 80/443。Gitea SSH 默认发布 2222，运维 SSH 使用原有端口和来源限制。云安全组与主机防火墙都需按实际用途配置；应用后端端口不直接向公网开放。

## 2. 放置源码并准备私有配置

在尚未存在 `/app/open-assets` 的服务器上执行：

```sh
sudo git clone https://github.com/soulteary/open-assets.git /app/open-assets
sudo -i
cd /app/open-assets
git rev-parse HEAD
```

后续示例均在这个管理员 shell 中执行。正式部署应记录所用提交；升级时先查看差异，不在运行目录直接覆盖私有文件。

先决定公共 DNS 基域和节点名。下面的 `lab.dev` 必须替换为自己控制的域名，`node-name` 替换为自己的节点名，再执行：

```sh
NODE_NAME=node-name PUBLIC_BASE=lab.dev python3 scripts/prepare-private.py
```

节点名可以使用 `beijing-1` 这样的标识。节点名使用 1–48 位小写字母、数字或连字符，首尾不能是连字符；不传 `NODE_NAME` 时使用 `node-name`。脚本生成七个主机名并保存到 `private/domains.json`：

```text
oss-node-name.<base>
img-node-name.<base>
storage-node-name.console.<base>
traefik-node-name.console.<base>
auth-node-name.console.<base>
git-node-name.console.<base>
info-node-name.console.<base>
```

上述 `node-name` 对应 `NODE_NAME`，`<base>` 对应 `PUBLIC_BASE`。两者同时写入私有 `.env`，`STORAGE_DOMAIN` 和 `GIT_DOMAIN` 写入解析后的完整主机名。

该脚本会生成密码、Publisher SSH 密钥、内部 CA 和证书，写入 `.env`、Stargate 配置及初始仓库模板。这些文件包含秘密，只保存在服务器和受保护的备份中，不上传 Git 或公开日志。发现已有 `.env`、`private/` 或非空数据目录时，脚本会拒绝操作。准备中断会保留 `private/.preparation.json`；不要直接删除标记后重跑。已有部署修改配置请按[配置说明](configuration.md)处理。

创建指向服务器的 DNS 记录。脚本不调用 DNS 服务商 API，也不申请公网证书。公网证书由现有 Traefik 的 resolver 处理；它使用的 DNS 凭据和 ACME 文件仍由原部署管理，不复制到开源仓库。

检查私有 `.env` 中的域名和镜像引用。示例镜像固定了版本和摘要，不在此步骤改用浮动 latest。检查 UID 是否适合当前数据目录：Gitea 20001、OtterIO 20002、OC 20004、Publisher/Nginx 20010。

## 3. 构建并初始化后端

```sh
docker compose config --quiet
docker compose pull otterio oc stargate gitea nginx
docker compose build publisher
docker compose up -d --no-build otterio gitea stargate oc
docker compose ps
docker compose logs --tail=50 otterio gitea oc
```

先确认日志没有启动错误，OtterIO 已出现 `IAM initialization complete`。再检查 Gitea 和 Stargate 的健康接口；两次请求均成功后执行初始化。若请求失败，先看对应服务日志，不要跳过检查：

```sh
curl --fail http://127.0.0.1:13000/api/healthz
curl --fail http://127.0.0.1:18080/healthz
python3 scripts/initialize-services.py
```

`initialize-services.py` 创建 Gitea 的 `content` 管理账号、空私有仓库 `content/images`、只读 Deploy Key 和 Push Hook，记录实际仓库 ID，并初始化 `attachments` 桶、匿名读取策略和发布/OC 用户。空仓库此时没有内容提交，稍后由你完成第一次 Push。它也从实际 Gitea 数据目录读取 SSH 公钥，写入 Publisher 的 known_hosts。

初始化必须来自成功完成的首次准备。程序在修改账号、策略和仓库前写入 `private/.initialization.json`；已有初始化记录、旧版部署或中断状态会被拒绝，没有覆盖开关。失败时保留现场并检查私有日志与备份，按实际阶段修复；可丢弃测试环境可销毁后重建。后续新增仓库用 `repo ensure`。

生成图片服务配置，再启动发布链路：

```sh
docker compose run --rm --no-deps \
  -v "$PWD/private/generated:/generated" publisher render /generated
install -d -o root -g 20010 -m 0750 /data/asset-stack/info /data/asset-stack/info-refresh
docker compose up -d --no-build nginx webhook publisher info
docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -t
```

此时后端和发布服务已启动，还需完成后面的 Traefik 接入。首次发布前没有当前图片目录，图片请求返回 404 是预期行为；不要通过重跑初始化处理这个 404。

## 4. 将现有 Traefik 接入网络

这一节修改的是现有 Traefik 的 Compose 文件，不是 Open Assets 的 `compose.yaml`。将下面内容合并到原配置，保留原有服务参数、网络和挂载。前一步启动应用后，这两个外部网络应已存在：

```yaml
services:
  traefik:
    networks:
      open_assets_edge: {}
      open_assets_auth:
        ipv4_address: 172.29.19.2

networks:
  open_assets_edge:
    external: true
    name: open-assets-edge
  open_assets_auth:
    external: true
    name: open-assets-auth
```

按原 Traefik 项目的部署方式应用变更。不要在 Open Assets 目录启动第二个争用 80/443 的 Traefik。临时 docker network connect 可以用于排查，但正式配置要写回 Compose，避免重建后丢失。

确认原 Traefik 满足以下条件：

- HTTPS entrypoint 名为 websecure，ACME resolver 名为 le；其他名称需先调整 configure-edge.py。
- file provider 监视一个挂载的动态配置目录，容器能读该目录。
- 已启用 Dashboard/API，未启用 api.insecure；公网管理请求只通过后面的认证路由访问。
- DNS provider 凭据、ACME 账户和状态文件继续使用原路径。不要清空状态，也不要让两个实例并发写同一文件。

## 5. 生成路由并核对 HTTPS

在 Open Assets 管理员 shell 中设置现有动态目录的宿主机路径：

```sh
export TRAEFIK_DYNAMIC_DIR=/path/to/existing/traefik/dynamic
python3 scripts/configure-edge.py \
  --domains private/domains.json \
  --ca private/tls/ca.crt \
  --output "$TRAEFIK_DYNAMIC_DIR/open-assets.yaml"
```

先替换路径，再执行上面的命令；应与 Traefik file provider 实际监视的挂载目录一致。脚本输出 JSON 语法的 YAML 文件，扩展名必须为 .yaml 或 .yml；Traefik 不会按该配方加载 .json 后缀的动态文件。生成文件可能与现有 router/service 名称冲突，应用前检查并处理原配置。

公开图片指向 Nginx 8080，公开对象只匹配 `/attachments/repositories/` 的 GET/HEAD。四个管理入口使用 Stargate ForwardAuth，Gitea、OC 继续保留自身身份验证。

Traefik 到 OC 和 OtterIO 使用内部 HTTPS，校验私有 CA 和主机名。Stargate 会话交换使用精确的 `/_session_exchange` 路由；认证后的 `/__auth_return` 返回固定同源导航文档，避免 OC 拒绝跨站认证重定向链。这两条路由不能改成不受主机名约束的全局规则，不能用删除 Origin 或 Sec-Fetch-* 的方式替代。

从管理电脑访问 private/domains.json 中的实际地址，确认浏览器信任证书；不要跳过浏览器证书警告。登录方法见 [使用指南](usage.md#1-查看地址和登录凭据)。

## 6. 首次发布及验收

按[首次分享指南](first-share.md)配置个人 SSH 公钥，克隆 content/images，生成示例图片与文本附件，再 Push 到 main，并从匿名客户端下载校验。初始化脚本不创建内容提交，也不自动设置分支保护；首次 Push 后按团队要求保护 main 并禁用 force push。OC 网页上传是独立的存储管理方式，不会触发这条 Git 发布链路，见[网页管理文件](usage.md#通过网页管理文件)。

检查以下结果：

- Webhook 投递成功且目标 SHA 有任务，最终状态为 published。
- 图片固定 URL 与附件 SHA URL 均能下载，文件字节与 Git 一致。
- 匿名不能列桶、写对象或访问管理 API；管理页面先进入 Stargate。
- Gitea 自身登录、OC IAM 登录、上传、下载及退出正常。
- 如果复用旧 Traefik，原有服务和证书续期配置仍能工作。

保留部署提交、镜像摘要、私有配置备份及验证结果。项目已有的 [验收记录](acceptance.md) 描述具体环境和范围，不能替代新环境的检查。

## 证书和后续维护

公网证书由现有 Traefik 续期。prepare-private.py 使用 Certs Maker 生成内部叶证书和独立 CA，两者有效期均为 3650 天，项目没有内部证书自动续期任务；到期前需重新签发、核对权限并重启相应服务。

首次发布通过后，再完成日常维护配置：

- 按 [Info 看板](info.md)安装采集服务。只启动 Info 容器还不会生成完整主机与容量快照。
- 按[日志管理](logging.md)配置宿主日志轮转，并按[运行维护](operations.md)安排备份和恢复演练。
- 运行 `./scripts/open-assets doctor`，按提示检查必需定时器和缓存。它不会自动修复，也不能替代上面的真实发布验收。
- 自动健康恢复和历史清理是可选功能，分别按[健康检查](health.md)、[本地保留](retention.md)和[对象保留](object-retention.md)评估后启用；不要为了消除提示直接开启删除。

日常登录、上传与发布见[使用指南](usage.md)，其他仓库通过[接入 CLI](cli.md)配置，调整发布规则见[配置说明](configuration.md)。组件升级及内部证书更换见[升级说明](upgrade.md)。

## 在隔离环境验证文档

需要复查首次部署时，使用独立 Docker daemon，不复制原有 private 或数据。仓库提供 [从零部署脚本](../tests/e2e/README.md)，会执行初始化、两次 CLI 接入及真实 Push 发布。脚本拒绝已有配置、数据或容器，默认不执行；它不是现有服务器的重装命令。实测环境和未覆盖范围见 [隔离部署记录](fresh-install.md)。
