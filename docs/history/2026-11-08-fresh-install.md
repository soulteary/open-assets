# 隔离环境从零部署记录（2026-11-08）

源码基线为 `6c76715354e3075e1298017863de007c9b7d97f8`，另包含本次对 `tests/e2e/verify.py` 的 CLI 调用修正。运行命令为 `OPEN_ASSETS_FRESH_TEST=1 sh tests/e2e/fresh-stack.sh`，完整流程退出码为 0。

## 环境与隔离

在本机 Linux 容器运行独立 Docker-in-Docker daemon，只监听容器内 Unix socket。不发布宿主机端口，不挂载宿主机目录或 Docker socket，不复制原部署的配置、数据、SSH、DNS 或 ACME 凭据。运行前 daemon 没有应用容器，应用 private 和数据目录均不存在。本次未连接或操作已有部署服务器。

测试环境为 Linux aarch64、Docker Engine 28.5.2、Compose 2.40.3、宿主脚本 Python 3.12.16，应用镜像 Python 3.13.18。组件使用基线 `.env.example` 固定的镜像摘要；应用从本次源码构建。测试容器使用 Alpine，宿主依赖由 apk 安装，因此本次没有验证部署文档中的 Ubuntu apt 安装命令。

首次依赖下载遇到软件源连接中断，重试后完成；应用初始化前未修改生成逻辑、固定版本或校验要求。

## 结果

- 首次准备生成新的密码、SSH 密钥、内部 CA 和私有配置；初始化创建初始仓库、桶与身份，Nginx 配置检查通过。
- 对已有状态再次执行准备和初始化均被拒绝，已生成私有文件和 `.env` 的 SHA-256 未改变。
- 通过宿主机 `scripts/open-assets repo ensure` 新增仓库，再执行相同命令：仓库 ID 不变，只读 Deploy Key 和 Webhook 各一个，Publisher 启动时间不变。
- CLI 拒绝占用已有根图片前缀的新仓库请求，Gitea 未创建该仓库。
- 使用新生成的个人 SSH 密钥克隆两个空仓库，执行使用指南中的首次提交和后续更新命令。四次真实 Push 经签名 Webhook 入队，四个任务全部为 `published`。
- 通过独立 Traefik 测试入口逐字节核对两个仓库的图片与版本附件；中文、空格、加号、百分号路径和图片 Range 请求通过。
- 匿名管理请求跳转 Stargate；密码登录和固定落地路径通过，认证后可读取 Traefik API。
- 结束时七个应用容器均为 healthy；独立 Traefik 测试容器运行中，未配置 Docker healthcheck，不将其运行状态写成健康检查通过。

## 本次修正与范围

旧脚本在仓库接入部分直接调用 `Manager.ensure`，没有覆盖用户实际使用的宿主 CLI。本次改为调用 `scripts/open-assets repo ensure`，用于新增、重复接入及冲突请求；查询资源仍使用管理 API。修正后的完整流程通过，未发现需要修改部署或发布业务代码的问题。

测试使用 `acceptance.invalid` 和临时 CA 文件证书，不调用公共 DNS 或 ACME。它不覆盖云安全组、真实浏览器、Info 与 systemd 定时任务安装、压力测试、升级降级及全部故障恢复场景。不能把本次结果作为上述功能重新验收的证明。

复现前提和清理方法见[端到端测试说明](../../tests/e2e/README.md)。原始日志、临时凭据和测试数据不进入公开仓库。
