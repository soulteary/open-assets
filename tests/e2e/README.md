# 从零部署验收

这里的脚本在空的独立 Docker daemon 上创建完整测试栈。它会创建账号、密钥、仓库、网络和文件，不属于普通单元测试，不应对已有部署运行。

## 环境

在可丢弃的 Linux VM 或 Docker-in-Docker 容器内放置源码，路径为 `/app/open-assets`。安装 Docker Engine/Compose、Python 3、PyYAML、bcrypt、Git、OpenSSH 客户端、OpenSSL 和 curl。需要能拉取 .env.example 中的镜像，以及模板固定的 Traefik 镜像，并可下载固定版本的 Certs Maker 官方发行包。

只把源码复制进去，不复制真实 private、.env、SSH 配置、ACME 状态或云凭据。测试 Docker daemon 不应发布远程 API，不要挂载生产或宿主机 Docker socket。容器内运行 DinD 需要额外权限，应把它限制在专用测试环境中。

以下命令必须在这个环境内执行：

```sh
cd /app/open-assets
OPEN_ASSETS_FRESH_TEST=1 sh tests/e2e/fresh-stack.sh
```

脚本拒绝已有 private、数据目录或容器。该检查不能自动证明你的 Docker context 是正确的，运行前仍需确认连接的是专用 daemon。正常部署目录不能靠删除检查文件来满足前提。

## 覆盖范围

流程按部署文档执行 prepare-private、镜像构建、后端就绪检查、initialize-services、Nginx 配置生成和发布服务启动。随后：

- CLI 新建 content/e2e-images，再次执行，检查仓库 ID、Key/Hook 数量和 Worker 启动时间。
- 请求冲突前缀，确认没有创建对应仓库。
- 使用新生成的个人 SSH 密钥克隆初始图片仓库和新增仓库，各执行一次真实 Push。
- 等待两个签名事件对应任务达到 published。
- 用独立测试 Traefik 检查图片、不可变附件、中文/特殊字符路径、Range 和管理入口认证。

测试域名全部位于保留的 acceptance.invalid 下。入口使用临时 CA 签发的文件证书，路由基于 configure-edge.py 生成，但测试副本移除 ACME resolver。它不验证真实 DNS、云安全组、公共证书申请或浏览器 UI；这些范围与单机生产验收记录分开。

结果写在 `private/e2e/result.json`，诊断文件和生成凭据也只在私有测试目录。公开保存结果前需检查内容，不上传整个 private。

## 退出与清理

测试不自动删除环境，以便检查失败。完成后销毁专用 VM/容器及其独立数据卷即可。不要在不确定 Docker context 时使用全局 prune 或批量删除命令。
