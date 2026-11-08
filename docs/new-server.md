# 从全新服务器开始

这条路线用于没有 Traefik、没有 Open Assets 数据的服务器。它使用独立 Traefik、文件路由和 HTTP-01 证书申请，不需要 DNS API 密钥。已经运行 Traefik 的服务器请直接使用[现有入口部署说明](deployment.md)，不要再启动一套争用 80/443 的入口。

先阅读[环境要求](requirements.md)。以下以 Ubuntu 24.04 LTS 为例，所有命令都在服务器执行，个人电脑上的首次发布放在最后。

## 1. 安装 Docker

下面只适用于新系统。如果已有 Docker/containerd 或其他容器服务，按 [Docker 官方说明](https://docs.docker.com/engine/install/ubuntu/)检查冲突和迁移，不直接套用这组安装命令。

```sh
sudo apt update
sudo apt install ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
sudo tee /etc/apt/sources.list.d/docker.sources > /dev/null <<EOF_DOCKER
Types: deb
URIs: https://download.docker.com/linux/ubuntu
Suites: $(. /etc/os-release && echo "${UBUNTU_CODENAME:-$VERSION_CODENAME}")
Components: stable
Architectures: $(dpkg --print-architecture)
Signed-By: /etc/apt/keyrings/docker.asc
EOF_DOCKER
sudo apt update
sudo apt install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
sudo systemctl enable --now docker
sudo docker version
sudo docker compose version
```

最后两个命令应正常返回版本。Docker 安装本身不会安装 Open Assets，也不会初始化存储。

## 2. 准备并初始化 Open Assets

继续完成[部署说明第 1 至第 3 节](deployment.md#1-检查环境)：安装宿主依赖、检查数据盘与网络、克隆源码、用自己的 `NODE_NAME`/`PUBLIC_BASE` 准备配置，再构建和初始化后端。完成第 3 节后回到本文，不执行该文档第 4 至第 6 节。

这段流程是共用的初始化步骤，不应重复执行。只有首次准备与初始化成功，才继续；已有或中断状态按保护机制拒绝时先保留现场。

在服务器管理员 shell 查看生成地址：

```sh
sudo -i
cd /app/open-assets
cat private/domains.json
```

在 DNS 中为这七个完整主机名添加 A 记录，指向服务器公网 IPv4。只有配置好可达的 IPv6 时才添加 AAAA；错误 AAAA 可能使访问与 ACME 校验失败。放行公网 TCP 80/443，按需要限制 SSH/2222 的来源。不要只配置一级通配符而漏掉 `.console` 管理域名。

HTTP-01 需要公共 CA 能从互联网访问 TCP 80，不能用仅本机 hosts 记录替代公共 DNS。如果网络条件无法满足，需要改用自行配置的 DNS-01 入口；不要通过跳过证书检查继续。

## 3. 准备独立 Traefik

确认没有其他程序占用 80/443，且 `/app/open-assets-traefik` 尚不存在。下面只创建新目录，不迁移既有入口：

```sh
cd /app/open-assets
ss -lntp
# 确认 80/443 空闲后执行；目录存在时先检查，不继续复制覆盖。
test ! -e /app/open-assets-traefik || { echo '目录已存在，先检查原部署。' >&2; exit 1; }
install -d -o root -g 20003 -m 0750 /app/open-assets-traefik
install -d -o root -g 20003 -m 0750 /app/open-assets-traefik/dynamic
install -d -o 20003 -g 20003 -m 0700 /app/open-assets-traefik/acme
install -o 20003 -g 20003 -m 0600 /dev/null /app/open-assets-traefik/acme/acme.json
install -m 0644 config/traefik/compose.example.yaml /app/open-assets-traefik/compose.yaml
install -m 0644 config/traefik/traefik.example.yaml /app/open-assets-traefik/traefik.yaml
```

编辑 `/app/open-assets-traefik/traefik.yaml`，将 `replace-me@example.org` 替换为自己的可用邮箱。文件配置了 `websecure` 和 `le`，与路由生成器匹配。HTTP 自动跳转公开的 HTTPS 443 端口；内部 8080/8443 不对用户暴露，配置依据见 [Traefik entryPoints](https://doc.traefik.io/traefik/v3.7/reference/install-configuration/entrypoints/)。

容器使用 UID 20003，不挂载 Docker socket；Dashboard/API 不启用 insecure。`ping` 入口只在容器内提供健康检查，不发布宿主机端口。

## 4. 生成路由并启动 HTTPS

```sh
cd /app/open-assets
python3 scripts/configure-edge.py \
  --domains private/domains.json \
  --ca private/tls/ca.crt \
  --output /app/open-assets-traefik/dynamic/open-assets.yaml
chmod 0644 /app/open-assets-traefik/dynamic/open-assets.yaml
docker compose --env-file /app/open-assets/.env \
  -f /app/open-assets-traefik/compose.yaml config --quiet
docker compose --env-file /app/open-assets/.env \
  -f /app/open-assets-traefik/compose.yaml up -d
docker compose --env-file /app/open-assets/.env \
  -f /app/open-assets-traefik/compose.yaml ps
docker compose --env-file /app/open-assets/.env \
  -f /app/open-assets-traefik/compose.yaml logs --tail=100 traefik
```

Compose 从 Open Assets 的私有 `.env` 读取固定 `TRAEFIK_IMAGE`，入口项目名为 `traefik`，与默认 `TRAEFIK_PROJECT` 一致。外部网络由前面启动的应用创建，不能跳过初始化直接启动入口。

首次申请证书需要时间。检查日志中的 DNS、HTTP-01 和路由错误，再从自己的电脑打开实际管理 HTTPS 地址，确认没有证书警告且匿名请求进入 Stargate。图片尚未发布时 404 正常；证书错误、502 或认证失败不是预期结果，按[故障排查](troubleshooting.md)处理。

`acme/acme.json` 包含账户和证书私钥，需随私有配置加密备份；保持 0600，仅运行一个写入实例。重复部署、修复或更新时不要再执行创建空 acme.json 的命令，也不要删掉证书状态重试。

## 5. 分享第一个文件

打开[首次分享指南](first-share.md)，选择 Git 发布或网页上传。Git 路线不需要额外公开配置；网页路线需先由管理员完成独立公开目录配置。成功标准是匿名客户端能获取预期文件，内容校验一致，而不只是浏览器显示“上传成功”。

最后完成[部署后的维护配置](deployment.md#证书和后续维护)：安装 Info 采集、日志轮转、安排备份并运行 doctor。历史清理和自动修复按需要启用，不是第一次分享的前置条件。
